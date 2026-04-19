//! Más constantes y valores ABI del `eclipse_kernel` no incluidos en `policy.rs`.
//! Mantener sincronizado con los archivos citados en cada bloque.

use crate::policy;

// --- `eclipse_kernel/src/memory.rs`: direcciones fijas ---

pub const PHYS_MEM_OFFSET: u64 = 0xFFFF_9000_0000_0000;
pub const KERNEL_OFFSET: u64 = 0xFFFF_8000_0000_0000;
pub const MMIO_VADDR_BASE: u64 = 0xFFFF_FA00_0000_0000;
pub const FB_VADDR_BASE: u64 = 0xFFFF_FB00_0000_0000;

pub const ANON_MMAP_PHYS_START: u64 = 0x1_0000_0000;
pub const ANON_MMAP_PHYS_END: u64 = ANON_MMAP_PHYS_START + (256u64 * 1024 * 1024 * 1024);

pub const GPU_FW_PHYS_BASE: u64 = 0x2000_0000;
pub const GPU_RPC_PHYS_BASE: u64 = 0x2200_0000;

/// `memory.rs` — `GPU_FB_VADDR_BASE`
pub const GPU_FB_VADDR_BASE: u64 = 0x0000_0002_0000_0000;

/// Pila de frames físicos libres para anon mmap (`FREE_FRAMES_STACK`)
pub const FREE_FRAMES_STACK_CAP: usize = 1024;

// --- `eclipse_kernel/src/memory.rs`: flags de página ---

pub const PAGE_PRESENT: u64 = 1 << 0;
pub const PAGE_WRITABLE: u64 = 1 << 1;
pub const PAGE_USER: u64 = 1 << 2;
pub const PAGE_WRITE_THROUGH: u64 = 1 << 3;
pub const PAGE_CACHE_DISABLE: u64 = 1 << 4;
pub const PAGE_ACCESSED: u64 = 1 << 5;
pub const PAGE_DIRTY: u64 = 1 << 6;
pub const PAGE_HUGE: u64 = 1 << 7;
pub const PAGE_GLOBAL: u64 = 1 << 8;
pub const PAGE_PAT_HUGE: u64 = 1 << 12;

/// Máscara de `PageTableEntry::get_addr` (alineación 4 KiB)
pub const PTE_ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

// --- `eclipse_kernel/src/syscalls.rs` ---

pub const SEEK_SET: u64 = 0;
pub const SEEK_CUR: u64 = 1;
pub const SEEK_END: u64 = 2;

pub const LAST_EXEC_ERR_LEN: usize = 80;
pub const KERNEL_HALF: u64 = 0xFFFF_8000_0000_0000;

pub const LINUX_AT_FDCWD: u64 = (-100_i64) as u64;

pub const SOL_SOCKET_LEVEL: i32 = 1;
pub const SCM_RIGHTS_TYPE: i32 = 1;

// --- `eclipse_kernel/src/elf_loader.rs` ---

pub const USER_ADDR_MAX: u64 = policy::SYS_MMAP_MAX_LENGTH;

pub const MIN_ENTRY_POINT: u64 = 0x80;
pub const DYNAMIC_MAIN_LOAD_BIAS: u64 = 0x4000_0000;
pub const DYNAMIC_INTERP_GAP: u64 = 0x1000_0000;

/// `MINIMAL_ENVP.len()` en `elf_loader.rs` (6 cadenas).
pub const MINIMAL_ENVP_COUNT: usize = 6;

pub const ARGV0_BUF_LEN: usize = 20;

pub const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];

pub const PT_LOAD: u32 = 1;
pub const PT_INTERP: u32 = 3;
pub const PT_DYNAMIC: u32 = 2;
pub const ET_DYN: u16 = 3;

// --- `eclipse_kernel/src/progress.rs` ---

pub const HISTORY_LINES: usize = 8;
pub const HISTORY_LINE_LEN: usize = 64;

// --- `eclipse_kernel/src/boot.rs` ---

pub const VIRTIO_DISPLAY_RESOURCE_ID: u32 = 2;

pub const KERNEL_CODE_SELECTOR: u16 = 0x08;
pub const KERNEL_DATA_SELECTOR: u16 = 0x10;
pub const USER_CODE_SELECTOR: u16 = 0x18 | 3;
pub const USER_DATA_SELECTOR: u16 = 0x20 | 3;
pub const TSS_SELECTOR: u16 = 0x40;

// --- `eclipse_kernel/src/virtio.rs` ---

pub const VIRTIO_MMIO_BASE: u64 = 0x0A00_0000;
pub const VIRTIO_MAGIC: u32 = 0x7472_6976;

pub const VIRTIO_ID_NET: u32 = 1;
pub const VIRTIO_ID_BLOCK: u32 = 2;
pub const VIRTIO_ID_GPU: u32 = 16;

pub const VIRTIO_STATUS_ACKNOWLEDGE: u32 = 1;
pub const VIRTIO_STATUS_DRIVER_OK: u32 = 4;
pub const VIRTIO_STATUS_FAILED: u32 = 128;

pub const VIRTIO_GPU_MAX_SCANOUTS: usize = 16;
pub const VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM: u32 = 1;
pub const CURSOR_RESOURCE_ID: u32 = 0x40;
pub const DISPLAY_BUFFER_RESOURCE_ID: u32 = 2;

pub const VIRTIO_CURSOR_WIDTH: u32 = 64;
pub const VIRTIO_CURSOR_HEIGHT: u32 = 64;

pub const VIRGL_MAX_CTX: u32 = 16;
pub const VIRGL_CTX_ID_MIN: u32 = 1;

pub const VIRTQ_AVAIL_RING_CAP: usize = 256;

pub const STATUS_CHANGE_DELAY_CYCLES: u32 = 1000;

pub const VIRTIO_GPU_CMD_GET_DISPLAY_INFO: u32 = 0x0100;
pub const VIRTIO_GPU_CMD_SUBMIT_3D: u32 = 0x0207;
pub const VIRTIO_GPU_RESP_OK_NODATA: u32 = 0x1100;

// --- `eclipse_kernel/src/interrupts.rs` ---

pub const APIC_TIMER_VECTOR: u8 = 0xFE;
pub const TLB_SHOOTDOWN_VECTOR: u8 = 0xFD;
pub const RESCHEDULE_IPI_VECTOR: u8 = 0xFC;
pub const GPU_INTERRUPT_VECTOR: u8 = 0x40;
pub const USB_INTERRUPT_VECTOR: u8 = 0x41;

pub const IRQ_HANDLER_SLOTS: usize = 16;

// --- `eclipse_kernel/src/ahci.rs` / `nvme.rs` ---

pub const DISK_SECTOR_SIZE: usize = 512;
pub const AHCI_BLOCK_SIZE: usize = 4096;
pub const AHCI_SECTORS_PER_BLOCK: u64 = (AHCI_BLOCK_SIZE / DISK_SECTOR_SIZE) as u64;

pub const NVME_ECLIPSE_BLOCK_SIZE: usize = 4096;

// --- `eclipse_kernel/src/ata.rs` ---

pub const ATA_POLL_LIMIT: usize = 5_000_000;

// --- `eclipse_kernel/src/filesystem.rs` ---

pub const DEVDIR_LIST_ID: usize = 0xFFFF;

// --- `eclipse_kernel/src/process.rs` ---

pub const SIGNAL_HANDLERS_COUNT: usize = 64;
pub const PROCESS_NO_CPU: u32 = u32::MAX;

// --- `eclipse_kernel/src/net.rs` ---

pub const NET_MAGIC: [u8; 4] = *b"NETW";

// --- `eclipse_kernel/src/sw_cursor.rs` ---

pub const SW_CURSOR_CELL_W: usize = 16;
pub const SW_CURSOR_CELL_H: usize = 24;
pub const SW_CURSOR_ARROW_H: usize = 16;

// --- `eclipse_kernel/src/epoll.rs` (Linux ABI interno) ---

pub const EPOLL_CTL_ADD: usize = 1;
pub const EPOLL_CTL_DEL: usize = 2;
pub const EPOLL_CTL_MOD: usize = 3;

/// `USER_ADDR_MAX` debe coincidir con el tope de `sys_mmap` en `policy`.
#[inline]
pub fn user_addr_max_matches_mmap_cap() -> bool {
    USER_ADDR_MAX == policy::SYS_MMAP_MAX_LENGTH
}
