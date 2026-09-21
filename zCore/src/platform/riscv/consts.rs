// RISCV

/// 内核每个硬件线程的栈页数。
pub const STACK_PAGES_PER_HART: usize = 32;

/// 最大的对称多核硬件线程数量。
///
/// This is the number of boot-stack slots `select_stack` carves out of
/// `.bss.bootstack`, indexed by RAW hart id — so it must be greater than the
/// highest hart id the machine has, not merely the number of harts. It was 5,
/// the hart count of the fu740 board, while `zCore/Makefile` starts QEMU with
/// `-smp 6`: whenever SBI picked hart 5 as the boot hart, `select_stack` put
/// its stack pointer one slot past the end of the array and the hart's first
/// pushes landed on whatever the linker put next — which, per
/// `platform/riscv/linker.ld`, is `sbss`: the kernel counter arena and the
/// 2 MiB bootstrap heap. The machine then died before printing a single line,
/// on roughly one boot in six, which `Linux Other Test Baremetal (riscv64)`
/// reported as a scattering of 60-second TIMEOUTs that moved from case to case
/// between runs.
///
/// 8 covers that `-smp 6` with room to spare, at 128 KiB of `.bss` per unused
/// slot. A hart past the end is parked by `select_stack` rather than given a
/// stack that is not there.
pub const MAX_HART_NUM: usize = 8;

#[inline]
pub fn phys_to_virt_offset() -> usize {
    kernel_mem_info().offset()
}

use spin::Once;

/// 内核位置信息
pub struct KernelMemInfo {
    /// 内核在物理地址空间的起始地址。
    pub paddr_base: usize,

    /// 内核所在虚拟地址空间的起始地址。
    ///
    /// 实际上是虚地址空间的最后一个 GiB 页的起始地址，
    /// 并与物理内存保持 2 MiB 页内偏移对齐。
    /// 与链接时设定的地址保持一致。
    pub vaddr_base: usize,

    /// 内核链接区域长度。
    pub size: usize,
}

impl KernelMemInfo {
    /// 初始化物理内存信息。
    ///
    /// # Safety
    ///
    /// 为了获取内核的物理地址，
    /// 这个函数必须在 `pc` 仍在物理地址空间时调用！
    unsafe fn new() -> Self {
        extern "C" {
            fn start();
            fn end();
        }
        let paddr_base = start as *const () as usize;
        let vaddr_base = 0xffff_ffc0_8020_0000;
        Self {
            paddr_base,
            vaddr_base,
            size: end as *const () as usize - paddr_base,
        }
    }

    /// 计算内核虚存空间到物理地址空间的偏移。
    #[inline]
    pub fn offset(&self) -> usize {
        self.vaddr_base - self.paddr_base
    }
}

static KERNEL_MEM_INFO: Once<KernelMemInfo> = Once::new();

#[inline]
pub fn kernel_mem_info() -> &'static KernelMemInfo {
    KERNEL_MEM_INFO.wait()
}

#[inline]
pub(super) unsafe fn kernel_mem_probe() -> &'static KernelMemInfo {
    KERNEL_MEM_INFO.call_once(|| KernelMemInfo::new())
}
