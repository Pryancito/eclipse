//! Constantes de layout, VirtIO, ELF, boot, almacenamiento y cursor — alineadas con `eclipse_kernel/`.

use kernel_host_tests::extended::*;
use kernel_host_tests::policy;

#[test]
fn memory_virtual_layout() {
    assert_eq!(PHYS_MEM_OFFSET, 0xFFFF_9000_0000_0000);
    assert_eq!(KERNEL_OFFSET, 0xFFFF_8000_0000_0000);
    assert_eq!(MMIO_VADDR_BASE, 0xFFFF_FA00_0000_0000);
    assert_eq!(FB_VADDR_BASE, 0xFFFF_FB00_0000_0000);
}

#[test]
fn anon_mmap_phys_span_256_gib() {
    assert_eq!(ANON_MMAP_PHYS_START, 0x1_0000_0000);
    assert_eq!(
        ANON_MMAP_PHYS_END - ANON_MMAP_PHYS_START,
        256u64 * 1024 * 1024 * 1024
    );
}

#[test]
fn gpu_phys_bases_and_fb_vaddr() {
    assert_eq!(GPU_FW_PHYS_BASE, 0x2000_0000);
    assert_eq!(GPU_RPC_PHYS_BASE, 0x2200_0000);
    assert_eq!(GPU_FW_PHYS_BASE + policy::GPU_FW_MAX_SIZE, 0x2200_0000);
    assert_eq!(GPU_FB_VADDR_BASE, 0x0000_0002_0000_0000);
}

#[test]
fn free_frames_stack_cap() {
    assert_eq!(FREE_FRAMES_STACK_CAP, 1024);
}

#[test]
fn page_flags_and_pte_mask() {
    assert_eq!(PAGE_PRESENT, 1);
    assert_eq!(PAGE_WRITABLE, 2);
    assert_eq!(PAGE_USER, 4);
    assert_eq!(PAGE_HUGE, 0x80);
    assert_eq!(PAGE_GLOBAL, 0x100);
    assert_eq!(PAGE_PAT_HUGE, 1 << 12);
    assert_eq!(PTE_ADDR_MASK.trailing_zeros(), 12);
}

#[test]
fn seek_whence_and_exec_err_buf() {
    assert_eq!(SEEK_SET, 0);
    assert_eq!(SEEK_CUR, 1);
    assert_eq!(SEEK_END, 2);
    assert_eq!(LAST_EXEC_ERR_LEN, 80);
    assert_eq!(KERNEL_HALF, KERNEL_OFFSET);
}

#[test]
fn linux_at_fdcwd_and_socket_ancil() {
    assert_eq!(LINUX_AT_FDCWD, 0xFFFF_FFFF_FFFF_FF9C);
    assert_eq!(SOL_SOCKET_LEVEL, 1);
    assert_eq!(SCM_RIGHTS_TYPE, 1);
}

#[test]
fn elf_user_address_space_cap() {
    assert!(user_addr_max_matches_mmap_cap());
    assert_eq!(MIN_ENTRY_POINT, 0x80);
    assert_eq!(DYNAMIC_MAIN_LOAD_BIAS, 0x4000_0000);
    assert_eq!(DYNAMIC_INTERP_GAP, 0x1000_0000);
    assert_eq!(MINIMAL_ENVP_COUNT, 6);
    assert_eq!(ARGV0_BUF_LEN, 20);
    assert_eq!(ELF_MAGIC, [0x7f, b'E', b'L', b'F']);
    assert_eq!(PT_LOAD, 1);
    assert_eq!(ET_DYN, 3);
}

#[test]
fn progress_history_grid() {
    assert_eq!(HISTORY_LINES, 8);
    assert_eq!(HISTORY_LINE_LEN, policy::LOG_CHAR_LIMIT);
}

#[test]
fn boot_gdt_selectors_and_virtio_display_id() {
    assert_eq!(VIRTIO_DISPLAY_RESOURCE_ID, DISPLAY_BUFFER_RESOURCE_ID);
    assert_eq!(VIRTIO_DISPLAY_RESOURCE_ID, 2);
    assert_eq!(KERNEL_CODE_SELECTOR, 0x08);
    assert_eq!(USER_CODE_SELECTOR, 0x1B);
    assert_eq!(USER_DATA_SELECTOR, 0x23);
    assert_eq!(TSS_SELECTOR, 0x40);
}

#[test]
fn virtio_ids_magic_status() {
    assert_eq!(VIRTIO_MAGIC, 0x7472_6976);
    assert_eq!(VIRTIO_ID_NET, 1);
    assert_eq!(VIRTIO_ID_BLOCK, 2);
    assert_eq!(VIRTIO_ID_GPU, 16);
    assert_eq!(VIRTIO_MMIO_BASE, 0x0A00_0000);
    assert_eq!(VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER_OK, 5);
    assert_eq!(VIRTIO_STATUS_FAILED, 128);
}

#[test]
fn virtio_gpu_scanouts_cursor_virgl() {
    assert_eq!(VIRTIO_GPU_MAX_SCANOUTS, 16);
    assert_eq!(VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM, 1);
    assert_eq!(CURSOR_RESOURCE_ID, 0x40);
    assert_eq!(VIRTIO_CURSOR_WIDTH, VIRTIO_CURSOR_HEIGHT);
    assert_eq!(VIRGL_MAX_CTX, 16);
    assert_eq!(VIRGL_CTX_ID_MIN, 1);
    assert_eq!(VIRTQ_AVAIL_RING_CAP, 256);
    assert_eq!(STATUS_CHANGE_DELAY_CYCLES, 1000);
}

#[test]
fn virtio_gpu_cmd_ids_monotonic_class() {
    assert!(VIRTIO_GPU_CMD_SUBMIT_3D > VIRTIO_GPU_CMD_GET_DISPLAY_INFO);
    assert!(VIRTIO_GPU_RESP_OK_NODATA > VIRTIO_GPU_CMD_SUBMIT_3D);
}

#[test]
fn interrupt_vectors_unique() {
    let v = [
        APIC_TIMER_VECTOR,
        TLB_SHOOTDOWN_VECTOR,
        RESCHEDULE_IPI_VECTOR,
        GPU_INTERRUPT_VECTOR,
        USB_INTERRUPT_VECTOR,
    ];
    for i in 0..v.len() {
        for j in i + 1..v.len() {
            assert_ne!(v[i], v[j], "vectores IDT duplicados");
        }
    }
    assert_eq!(IRQ_HANDLER_SLOTS, 16);
}

#[test]
fn disk_block_geometry() {
    assert_eq!(DISK_SECTOR_SIZE, 512);
    assert_eq!(AHCI_BLOCK_SIZE, policy::BLOCK_SIZE);
    assert_eq!(AHCI_SECTORS_PER_BLOCK, 8);
    assert_eq!(NVME_ECLIPSE_BLOCK_SIZE, 4096);
}

#[test]
fn ata_poll_limit() {
    assert_eq!(ATA_POLL_LIMIT, 5_000_000);
}

#[test]
fn filesystem_devdir_sentinel() {
    assert_eq!(DEVDIR_LIST_ID, 0xFFFF);
}

#[test]
fn process_signal_table_width() {
    assert_eq!(SIGNAL_HANDLERS_COUNT, 64);
    assert_eq!(PROCESS_NO_CPU, 0xFFFF_FFFF);
}

#[test]
fn net_magic_token() {
    assert_eq!(NET_MAGIC, *b"NETW");
}

#[test]
fn software_cursor_metrics() {
    assert_eq!(SW_CURSOR_CELL_W, 16);
    assert_eq!(SW_CURSOR_CELL_H, 24);
    assert_eq!(SW_CURSOR_ARROW_H, 16);
}

#[test]
fn epoll_ctl_opcodes() {
    assert_eq!(EPOLL_CTL_ADD, 1);
    assert_eq!(EPOLL_CTL_DEL, 2);
    assert_eq!(EPOLL_CTL_MOD, 3);
}
