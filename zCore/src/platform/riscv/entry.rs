use super::{
    boot_page_table::BootPageTable,
    consts::{kernel_mem_info, MAX_HART_NUM, STACK_PAGES_PER_HART},
};
use dtb_walker::{Dtb, DtbObj, HeaderError::*, Property, Str, WalkOperation::*};
use kernel_hal::hart_walk::{outcome, parse_hart_id, HartWalk, Next, Outcome, Skip};
use kernel_hal::KernelConfig;

/// 内核入口。
///
/// # Safety
///
/// 裸函数。
#[unsafe(naked)]
#[no_mangle]
#[link_section = ".text.entry"]
unsafe extern "C" fn _start(hartid: usize, device_tree_paddr: usize) -> ! {
    core::arch::naked_asm!(
        "call {select_stack}", // 设置启动栈
        "j    {main}",         // 进入 rust
        select_stack = sym select_stack,
        main         = sym primary_rust_main,
    )
}

/// 副核入口。此前副核被 bootloader/see 阻塞。
///
/// # Safety
///
/// 裸函数。
#[unsafe(naked)]
unsafe extern "C" fn secondary_hart_start(hartid: usize) -> ! {
    core::arch::naked_asm!(
        "call {select_stack}", // 设置启动栈
        "j    {main}",         // 进入 rust
        select_stack = sym select_stack,
        main         = sym secondary_rust_main,
    )
}

/// 启动页表
static mut BOOT_PAGE_TABLE: BootPageTable = BootPageTable::ZERO;

/// 主核启动。
extern "C" fn primary_rust_main(hartid: usize, device_tree_paddr: usize) -> ! {
    // 清零 bss 段
    extern "C" {
        static mut sbss: u64;
        static mut ebss: u64;
    }
    unsafe { r0::zero_bss(&raw mut sbss, &raw mut ebss) };
    // 使能启动页表
    let sstatus = unsafe {
        let page_table = &mut *core::ptr::addr_of_mut!(BOOT_PAGE_TABLE);
        page_table.init();
        page_table.launch()
    };
    let mem_info = kernel_mem_info();
    // 检查设备树
    let dtb = unsafe {
        Dtb::from_raw_parts_filtered((device_tree_paddr + mem_info.offset()) as _, |e| {
            matches!(e, Misaligned(4) | LastCompVersion(_))
        })
    }
    .unwrap();
    // 打印启动信息
    println!(
        "
boot page table launched, sstatus = {sstatus:#x}
kernel (physical): {:016x}..{:016x}
kernel (remapped): {:016x}..{:016x}
device tree:       {device_tree_paddr:016x}..{:016x}
",
        mem_info.paddr_base,
        mem_info.paddr_base + mem_info.size,
        mem_info.vaddr_base,
        mem_info.vaddr_base + mem_info.size,
        device_tree_paddr + dtb.total_size(),
    );
    // 启动副核
    //
    // `smp=off` has to be read from the device tree right here. The harts are
    // released before `primary_main` exists, so by the time the command line is
    // parsed into `SMP_ENABLED` every one of them is already running, and the
    // "single-core boot forced (smp=off)" the boot log then prints is a switch
    // the kernel says it obeyed and did not. x86_64 checks the same flag inside
    // `start_application_processors`, which runs late enough to just read it.
    if dtb_says_smp_off(&dtb) {
        kernel_hal::set_smp_enabled(false);
        println!("[smp] secondary harts not started (smp=off)");
    } else {
        boot_secondary_harts(
            hartid,
            &dtb,
            secondary_hart_start as *const () as usize - mem_info.offset(),
        );
    }
    // 转交控制权
    crate::primary_main(KernelConfig {
        phys_to_virt_offset: mem_info.offset(),
        dtb_paddr: device_tree_paddr,
        dtb_size: dtb.total_size() as _,
    });
    sbi_rt::system_reset(sbi_rt::Shutdown, sbi_rt::NoReason);
    unreachable!()
}

/// Whether the device tree's `/chosen/bootargs` asks for a single-core boot.
///
/// The answer is decided inside the walk rather than carried out of it: the
/// property's bytes live only as long as the callback. They are the raw,
/// NUL-terminated property, so `cmdline::from_c_bytes` strips the terminator —
/// without it the value of the *last* flag on the line reads as `"off\0"`,
/// which spells no boolean, and the flag looks unwritten.
fn dtb_says_smp_off(dtb: &Dtb) -> bool {
    let mut off = false;
    dtb.walk(|path, obj| match obj {
        DtbObj::SubNode { name } => {
            if path.is_root() && name == Str::from("chosen") {
                StepInto
            } else {
                StepOver
            }
        }
        DtbObj::Property(Property::General { name, value }) if name == Str::from("bootargs") => {
            if let Some(cmdline) = kernel_hal::cmdline::from_c_bytes(value) {
                off = kernel_hal::cmdline::is_off(cmdline, "smp");
            }
            Terminate
        }
        DtbObj::Property(_) => StepOver,
    });
    off
}

/// 副核启动。
extern "C" fn secondary_rust_main() -> ! {
    let _ = unsafe { (&*core::ptr::addr_of!(BOOT_PAGE_TABLE)).launch() };
    crate::secondary_main()
}

/// 根据硬件线程号设置启动栈。
///
/// The array is indexed by raw hart id, so a hart id at or past
/// [`MAX_HART_NUM`] has no slot. It is parked here rather than handed a stack
/// pointer past the end of the array, which is what used to happen and left
/// the hart pushing onto `sbss` — the counter arena and the bootstrap heap —
/// before the kernel had printed anything at all. The message goes out through
/// the SBI legacy console by hand: there is no stack yet, so nothing that
/// needs one can report this.
///
/// # Safety
///
/// 裸函数。
#[unsafe(naked)]
unsafe extern "C" fn select_stack(hartid: usize) {
    const STACK_LEN_PER_HART: usize = 4096 * STACK_PAGES_PER_HART;
    const STACK_LEN_TOTAL: usize = STACK_LEN_PER_HART * MAX_HART_NUM;
    #[link_section = ".bss.bootstack"]
    static mut BOOT_STACK: [u8; STACK_LEN_TOTAL] = [0u8; STACK_LEN_TOTAL];
    static NO_STACK_MSG: [u8; 58] =
        *b"hart id is past MAX_HART_NUM: no boot stack, parking it.\n\0";

    core::arch::naked_asm!(
        "   li   t2, {max_hart}
            bgeu a0, t2, 2f",
        "   mv   tp, a0",
        "   addi t0, a0,  1
            la   sp, {stack}
            li   t1, {len_per_hart}
         1: add  sp, sp, t1
            addi t0, t0, -1
            bnez t0, 1b
            ret
        ",
        // SBI legacy console putchar (EID 0x01), one byte at a time, then halt.
        "2: la   t0, {msg}
         3: lbu  a0, 0(t0)
            beqz a0, 4f
            li   a6, 0
            li   a7, 1
            ecall
            addi t0, t0, 1
            j    3b
         4: wfi
            j    4b
        ",
        stack        =   sym BOOT_STACK,
        msg          =   sym NO_STACK_MSG,
        len_per_hart = const STACK_LEN_PER_HART,
        max_hart     = const MAX_HART_NUM,
    )
}

// 启动副核
//
// Which harts this starts, and what a refusal from the SEE means, live in
// `kernel_hal::hart_walk` so the host test suite can reach them: the `ecall`
// is the only part of this that needs a riscv machine.
fn boot_secondary_harts(boot_hartid: usize, dtb: &Dtb, start_addr: usize) {
    if sbi_rt::probe_extension(sbi_rt::Hsm).is_unavailable() {
        println!("HSM SBI extension is not supported for current SEE.");
        return;
    }

    // `MAX_HART_NUM` bounds the raw hart id (the boot stack array `select_stack`
    // indexes); `MAX_CORE_NUM - 1` bounds how many secondaries the per-CPU
    // tables can hold on top of this one.
    let mut walk = HartWalk::new(
        boot_hartid,
        MAX_HART_NUM,
        kernel_hal::config::MAX_CORE_NUM.saturating_sub(1),
    );
    let mut cpus = false;
    dtb.walk(|path, obj| match obj {
        DtbObj::SubNode { name } => {
            if path.is_root() {
                if name == Str::from("cpus") {
                    // 进入 cpus 节点
                    cpus = true;
                    StepInto
                } else if cpus {
                    // 已离开 cpus 节点
                    let next = walk.see_cpu_node(None);
                    hart_start(&mut walk, next, start_addr);
                    Terminate
                } else {
                    // 其他节点
                    StepOver
                }
            } else if path.name() == Str::from("cpus") {
                // 如果没有 cpu 序号，肯定是单核的
                if name == Str::from("cpu") {
                    return Terminate;
                }
                if name.starts_with("cpu@") {
                    // A node name the firmware wrote. `parse_hart_id` answers
                    // `None` rather than panicking on one it will not vouch
                    // for; the walk then drops that node and reads the next.
                    let id = name.as_str().ok().and_then(parse_hart_id);
                    if id.is_none() {
                        println!("[smp] {name} does not name a hart — ignored");
                    }
                    let next = walk.see_cpu_node(id);
                    hart_start(&mut walk, next, start_addr);
                    StepInto
                } else {
                    StepOver
                }
            } else {
                StepOver
            }
        }
        // 状态不是 "okay" 的 cpu 不能启动
        //
        // Which strings count as startable is `see_status`' call and only its
        // call -- asking the same question here too is how the two answers
        // drift. A status whose bytes are not utf-8 is a status we cannot
        // trust, so it arrives as the empty string and the core stays asleep.
        DtbObj::Property(Property::Status(status)) if path.name().starts_with("cpu@") => {
            if let Some(id) = walk.see_status(status.as_str().unwrap_or("")) {
                println!("hart{id} has status: {status}");
            }
            StepOver
        }
        DtbObj::Property(_) => StepOver,
    });
    // The end of the walk. `dtb_walker` never announces it, and the code this
    // replaces flushed its pending hart only on the next root-level node after
    // `/cpus`: a blob whose `/cpus` is the root's last child lost its last core
    // on every boot and said nothing.
    let next = walk.finish();
    hart_start(&mut walk, next, start_addr);

    println!(
        "[smp] {} secondary hart(s) started, {} already up, {} absent, {} refused",
        walk.started(),
        walk.already_on(),
        walk.absent(),
        walk.failures(),
    );
    println!();
}

/// Issue one `HART_START`, or say why the walk refused to.
///
/// A refusal is never fatal here. It used to be: `panic!("start hart{hartid}
/// failed")` meant one core the SEE would not hand over took down a machine
/// with every other core waiting — which is why `entry64.rs` carries a
/// `board-fu740` `cfg` that skips hart 0 by hand. x86_64 and aarch64 both skip
/// the CPU and boot with the ones that came up; this is the third place the
/// kernel asks that question and it now answers it the same way.
fn hart_start(walk: &mut HartWalk, next: Next, start_addr: usize) {
    let hartid = match next {
        Next::Nothing => return,
        Next::Skip(hartid, why) => {
            // Every arm is braced: zCore's `println!` ends in a semicolon,
            // so it is a statement and not an expression.
            match why {
                Skip::BootHart => {
                    println!("hart{hartid} is the primary hart.");
                }
                Skip::NoStack => {
                    println!(
                        "hart{hartid} is past MAX_HART_NUM ({MAX_HART_NUM}) — no boot stack, \
                         not started"
                    );
                }
                Skip::NoRoom => {
                    println!("hart{hartid} has no per-CPU slot left — not started");
                }
            }
            return;
        }
        Next::Start(hartid) => hartid,
    };
    println!("hart{hartid} is booting...");
    let ret = sbi_rt::hart_start(hartid, start_addr, 0);
    let verdict = outcome(ret.error);
    walk.saw(verdict);
    match verdict {
        Outcome::Started => {}
        Outcome::AlreadyOn => {
            println!("hart{hartid} was already running");
        }
        Outcome::Absent => {
            println!("hart{hartid} is not this SEE's to start");
        }
        Outcome::Failed(code) => {
            println!("start hart{hartid} failed. error: {code}");
        }
    }
}
