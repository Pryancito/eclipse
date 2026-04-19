//! Suite de límites y fórmulas que deben coincidir con `eclipse_kernel/`.
//! Ejecutar: `cargo test` desde `kernel_host_tests/`.

use kernel_host_tests::policy::*;

// --- memoria / DMA / DRM / VirtIO ---

#[test]
fn dma_heap_cap_is_64_mib() {
    assert_eq!(MAX_KERNEL_DMA_HEAP_ALLOC, 64 * 1024 * 1024);
    assert_eq!(MAX_GEM_BUFFER_SIZE, MAX_KERNEL_DMA_HEAP_ALLOC);
}

#[test]
fn gpu_fw_and_rpc_caps() {
    assert_eq!(GPU_FW_MAX_SIZE, 32 * 1024 * 1024);
    assert_eq!(GPU_RPC_MAX_SIZE, 1 * 1024 * 1024);
}

#[test]
fn rejects_one_gib_framebuffer_dims() {
    assert!(virtio_bgra_framebuffer_bytes(16384, 16384).is_none());
}

#[test]
fn accepts_4k_framebuffer() {
    let (p, s) = virtio_bgra_framebuffer_bytes(3840, 2160).expect("4K");
    assert_eq!(p, 3840 * 4);
    assert_eq!(s, p * 2160);
    assert!(s < MAX_KERNEL_DMA_HEAP_ALLOC);
}

#[test]
fn virtio_net_tx_cap_16k() {
    assert_eq!(VIRTIO_NET_MAX_TX_BYTES, 16 * 1024);
}

#[test]
fn virgl_backing_cap_16_mib() {
    assert_eq!(VIRGL_ALLOC_BACKING_MAX, 16 * 1024 * 1024);
}

// --- syscalls ---

#[test]
fn execve_string_budget_is_4_mib() {
    assert_eq!(MAX_EXECVE_ARG_ENV_BYTES, 4 * 1024 * 1024);
}

#[test]
fn sys_read_write_get_logs_receive_caps() {
    assert_eq!(SYS_READ_MAX_BYTES, 32 * 1024 * 1024);
    assert_eq!(SYS_WRITE_MAX_BYTES, 1024 * 1024);
    assert_eq!(SYS_GET_LOGS_MAX_BYTES, 4096);
    assert_eq!(SYS_RECEIVE_MAX_BYTES, 4096);
}

#[test]
fn sys_mmap_length_boundary() {
    assert!(SYS_MMAP_MAX_LENGTH < u64::MAX);
    // El kernel rechaza `length > SYS_MMAP_MAX_LENGTH`.
    let over = SYS_MMAP_MAX_LENGTH.saturating_add(1);
    assert!(over > SYS_MMAP_MAX_LENGTH);
}

#[test]
fn virgl_submit_cap_256k() {
    assert_eq!(MAX_SUBMIT_SIZE, 256 * 1024);
}

#[test]
fn socket_buffer_and_passfd_caps() {
    assert_eq!(CONNECTION_BUFFER_CAP, 256 * 1024);
    assert_eq!(MAX_PASS_FDS, 8);
}

// --- ELF / exec heap copy ---

#[test]
fn elf_padding_boundary_matches_sys_exec_guard() {
    // `elf_size_allowed_for_kernel_heap_copy`: el tope es el tamaño **tras** redondeo a múltiplo de usize (8).
    let cap: u64 = 128 * 1024 * 1024;
    // 128 MiB − 8 bytes: ya múltiplo de 8 → padded < 128 MiB → permitido.
    assert!(elf_size_allowed_for_kernel_heap_copy(cap - 8));
    // A partir de aquí el redondeo pide un bloque de ≥ 128 MiB al allocator.
    assert!(!elf_size_allowed_for_kernel_heap_copy(cap - 7));
    for byte_len in (cap - 7)..cap {
        assert!(
            !elf_size_allowed_for_kernel_heap_copy(byte_len),
            "byte_len={byte_len} padded={}",
            elf_byte_len_heap_padded(byte_len)
        );
    }
    assert_eq!(elf_byte_len_heap_padded(cap - 8), (cap - 8) as usize);
    assert_eq!(elf_byte_len_heap_padded(cap - 7), cap as usize);
}

// --- filesystem ---

#[test]
fn filesystem_block_and_tlv_caps() {
    assert_eq!(BLOCK_SIZE, 4096);
    assert_eq!(MAX_RECORD_SIZE, 32 * 1024 * 1024);
    assert_eq!(MAX_VIRTUAL_FILE_SIZE, 64 * 1024 * 1024);
}

#[test]
fn whole_file_read_heap_guard() {
    assert!(!read_file_inode_too_large_for_heap(0)); // vacío se mane aparte en kernel
    let m = MAX_WHOLE_FILE_READ;
    assert!(!read_file_inode_too_large_for_heap(m - 8));
    assert!(read_file_inode_too_large_for_heap(m - 7));
    assert!(read_file_inode_too_large_for_heap(m));
}

#[test]
fn symlink_depth_cap() {
    assert_eq!(MAX_SYMLINK_DEPTH, 16);
}

// --- IPC mailbox (modelo) ---

#[test]
fn mailbox_depth_and_message_data() {
    assert_eq!(MAILBOX_DEPTH, 256);
    assert_eq!(MAX_MESSAGE_DATA, 512);
}

#[test]
fn mailbox_ring_fifo_and_full() {
    let mut mb = RingMailbox::new(MAILBOX_DEPTH);
    for i in 0u32..MAILBOX_DEPTH as u32 {
        assert!(mb.push(i));
    }
    assert!(!mb.push(999));
    assert_eq!(mb.len(), MAILBOX_DEPTH);
    for i in 0u32..MAILBOX_DEPTH as u32 {
        assert_eq!(mb.pop(), Some(i));
    }
    assert_eq!(mb.len(), 0);
    assert!(mb.pop().is_none());
}

// --- scheme SHM ---

#[test]
fn shm_ftruncate_cap_16_mib() {
    assert_eq!(SHM_FTRUNCATE_MAX, 16 * 1024 * 1024);
}

// --- syscalls: rutas, pilas, ioctl, cmsg ---

#[test]
fn path_limits_match_kernel() {
    assert_eq!(MAX_PATH_LENGTH, 1024);
    assert_eq!(SYSCALL_PATH_STRLEN_CAP, 4096);
}

#[test]
fn send_msg_cap_matches_message_data() {
    assert_eq!(SYS_SEND_MAX_MSG, MAX_MESSAGE_DATA);
    assert_eq!(SYS_SEND_MAX_MSG, 512);
}

#[test]
fn nvidia_ioctl_payload_cap() {
    assert_eq!(NVIDIA_IOCTL_MAX_PAYLOAD, 64);
}

#[test]
fn cmsg_hdr_size() {
    assert_eq!(CMSG_HDR_SIZE, 16);
}

#[test]
fn user_and_kernel_stack_sizes() {
    assert_eq!(USER_STACK_SIZE, 0x10_0000);
    assert_eq!(KERNEL_STACK_SIZE, 32768);
}

// --- fd / proceso / planificación / boot ---

#[test]
fn fd_table_dims() {
    assert_eq!(MAX_FDS_PER_PROCESS, 64);
    assert_eq!(MAX_FD_PROCESSES, 256);
    assert_eq!(MAX_PROCESSES, MAX_PIDS);
    assert_eq!(MAX_PROCESSES, 256);
}

#[test]
fn cpu_and_scheduler_caps_align() {
    assert_eq!(PROCESS_MAX_CPUS, 32);
    assert_eq!(SCHEDULER_MAX_CPUS, PROCESS_MAX_CPUS);
    assert_eq!(MAX_SMP_CPUS, PROCESS_MAX_CPUS);
    assert_eq!(SLEEP_QUEUE_SIZE, 256);
    assert_eq!(MAX_PIDS, 256);
}

#[test]
fn boot_df_stack() {
    assert_eq!(DF_STACK_SIZE, 8192);
}

// --- memoria / tablas de página ---

#[test]
fn page_table_and_kernel_region() {
    assert_eq!(PAGE_TABLE_ENTRIES, 512);
    assert_eq!(KERNEL_REGION_SIZE, 0x8000_0000);
}

// --- IPC: colas de servidor, mapa PID, recorte de payload ---

#[test]
fn ipc_server_queue_and_pid_map() {
    assert_eq!(SERVER_MESSAGE_QUEUE_LEN, 64);
    assert_eq!(PID_MAP_SIZE, 4096);
}

#[test]
fn ipc_payload_clip() {
    assert_eq!(ipc_clip_payload(0), 0);
    assert_eq!(ipc_clip_payload(512), 512);
    assert_eq!(ipc_clip_payload(513), 512);
    assert_eq!(ipc_clip_payload(usize::MAX), MAX_MESSAGE_DATA);
}

// --- interrupciones / pipe / fs caches ---

#[test]
fn input_and_pipe_buffers() {
    assert_eq!(KEY_BUFFER_SIZE, 256);
    assert_eq!(MOUSE_BUFFER_SIZE, 128);
    assert_eq!(PIPE_BUF_CAP, 65536);
}

#[test]
fn filesystem_cache_dims() {
    assert_eq!(INODE_CACHE_SIZE, 128);
    assert_eq!(DIR_CACHE_SIZE, 32);
}

// --- progreso / logs HUD ---

#[test]
fn progress_log_limits() {
    assert_eq!(LOG_BUF_SIZE, 128);
    assert_eq!(LOG_CHAR_LIMIT, 64);
    let s = "a".repeat(64);
    assert_eq!(progress_truncate_line_for_log(&s), s.as_str());
    let long = "b".repeat(65);
    let t = progress_truncate_line_for_log(&long);
    assert_eq!(t.len(), 64);
    assert!(t.chars().all(|c| c == 'b'));
}

// --- servers / elf / bcache / red / USB ---

#[test]
fn servers_queue_and_event() {
    assert_eq!(INPUT_EVENT_SIZE, 24);
    assert_eq!(MAX_QUEUE_BYTES, CONNECTION_BUFFER_CAP);
}

#[test]
fn elf_process_name_cap() {
    assert_eq!(MAX_PROCESS_NAME_LEN, 16);
}

#[test]
fn bcache_slots() {
    assert_eq!(BCACHE_CACHE_SIZE, 1024);
}

#[test]
fn e1000e_ring_and_packet_buf() {
    assert_eq!(E1000E_RX_RING_SIZE, 128);
    assert_eq!(E1000E_TX_RING_SIZE, 128);
    assert_eq!(E1000E_PACKET_BUF_SIZE, 2048);
}

#[test]
fn xhci_hid_slots_per_controller() {
    assert_eq!(XHCI_HID_ENDPOINT_SLOTS, 8);
}

/// Réplica de `usb_hid::tests::configure_endpoint_context_math_uses_context_size`.
#[test]
fn xhci_configure_endpoint_context_math_uses_context_size() {
    for &csz in &[32usize, 64usize] {
        assert_eq!(
            xhci_configure_endpoint_input_context_bytes(csz),
            33 * csz
        );
        let ep_id: usize = 3;
        assert_eq!(
            xhci_endpoint_context_offset(csz, ep_id),
            2 * csz + (ep_id - 1) * csz
        );
    }
}

// --- sync: empaquetado ReentrantMutex ---

#[test]
fn reentrant_mutex_pack_unpack_roundtrip() {
    for &(owner, depth) in &[(0i32, 0u32), (-1i32, 0u32), (1i32, 5u32), (31i32, 0xFFFF_FFFFu32)] {
        let p = reentrant_mutex_pack(owner, depth);
        assert_eq!(reentrant_mutex_unpack(p), (owner, depth));
    }
}
