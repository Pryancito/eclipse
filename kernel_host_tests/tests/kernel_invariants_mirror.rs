//! Espejo en **host** de `eclipse_kernel/src/invariants.rs`: mismas relaciones entre límites.
//! Si el kernel endurece o relaja políticas, actualizar **ambos** sitios.

use kernel_host_tests::extended;
use kernel_host_tests::policy;

#[test]
fn dma_gem_alignment() {
    assert_eq!(policy::MAX_GEM_BUFFER_SIZE, policy::MAX_KERNEL_DMA_HEAP_ALLOC);
}

#[test]
fn virtual_and_record_sizes_vs_dma_cap() {
    assert!(policy::MAX_VIRTUAL_FILE_SIZE <= policy::MAX_KERNEL_DMA_HEAP_ALLOC);
    assert!(policy::MAX_RECORD_SIZE <= policy::MAX_KERNEL_DMA_HEAP_ALLOC);
    assert!(policy::READ_FILE_ALLOC_MAX_CONTENT <= policy::MAX_RECORD_SIZE);
}

#[test]
fn fd_table_matches_process_table() {
    assert_eq!(policy::MAX_FD_PROCESSES, policy::MAX_PROCESSES);
    assert!(policy::MAX_FDS_PER_PROCESS > 0 && policy::MAX_FDS_PER_PROCESS <= 256);
}

#[test]
fn smp_cpu_counts_agree() {
    assert_eq!(policy::SCHEDULER_MAX_CPUS, policy::MAX_SMP_CPUS);
    assert_eq!(policy::PROCESS_MAX_CPUS, policy::MAX_SMP_CPUS);
    assert!(policy::MAX_SMP_CPUS >= 1 && policy::MAX_SMP_CPUS <= 64);
}

#[test]
fn pipe_fits_in_connection_buffer() {
    assert!(policy::PIPE_BUF_CAP <= policy::CONNECTION_BUFFER_CAP);
}

#[test]
fn block_size_is_4k_power_of_two() {
    assert_eq!(policy::BLOCK_SIZE, 4096);
    assert!(policy::BLOCK_SIZE.is_power_of_two());
}

#[test]
fn ipc_mailbox_and_message_sane() {
    assert!(policy::MAILBOX_DEPTH >= 16);
    assert!(policy::MAX_MESSAGE_DATA >= 64);
    assert!(policy::MAX_MESSAGE_DATA <= 4096);
}

#[test]
fn virgl_backing_within_dma_heap() {
    assert!(policy::VIRGL_ALLOC_BACKING_MAX <= policy::MAX_KERNEL_DMA_HEAP_ALLOC);
}

#[test]
fn elf_exec_padding_guard_matches_kernel() {
    let cap = 128usize * 1024 * 1024;
    assert!(policy::elf_byte_len_heap_padded((cap - 8) as u64) < cap);
    assert_eq!(policy::elf_byte_len_heap_padded((cap - 7) as u64), cap);
}

#[test]
fn user_addr_cap_matches_mmap_policy() {
    assert!(extended::user_addr_max_matches_mmap_cap());
}

#[test]
fn shm_region_within_dma_cap() {
    assert!(policy::SHM_REGION_MAX_BYTES <= policy::MAX_KERNEL_DMA_HEAP_ALLOC);
    assert_eq!(policy::SHM_REGION_MAX_BYTES, policy::SHM_FTRUNCATE_MAX);
}

#[test]
fn virgl_submit_within_dma_cap() {
    assert!(policy::MAX_SUBMIT_SIZE <= policy::MAX_KERNEL_DMA_HEAP_ALLOC);
}

#[test]
fn virtio_net_tx_within_socket_buffer() {
    assert!(policy::VIRTIO_NET_MAX_TX_BYTES <= policy::CONNECTION_BUFFER_CAP);
}

#[test]
fn execve_budget_within_dma_cap() {
    assert!(policy::MAX_EXECVE_ARG_ENV_BYTES <= policy::MAX_KERNEL_DMA_HEAP_ALLOC);
}

#[test]
fn sys_read_cap_matches_read_file_alloc() {
    assert_eq!(
        policy::SYS_READ_MAX_BYTES as usize,
        policy::READ_FILE_ALLOC_MAX_CONTENT
    );
}

#[test]
fn fds_per_process_lte_fd_table_width() {
    assert!(policy::MAX_FDS_PER_PROCESS <= policy::MAX_FD_PROCESSES);
}
