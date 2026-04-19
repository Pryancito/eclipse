//! Invariantes entre límites del kernel comprobadas en **tiempo de compilación**.
//! Si alguna `assert!` falla, `eclipse_kernel` no compila (garantía fuerte frente a desalineaciones).
//!
//! Espejo en host: `kernel_host_tests/tests/kernel_invariants_mirror.rs` y `tests/syscall_mmap_abi.rs`;
//! si cambias un límite aquí, mantén alineados `policy` / `extended` y esos tests.

use crate::boot::MAX_SMP_CPUS;
use crate::drm::MAX_GEM_BUFFER_SIZE;
use crate::fd::{MAX_FD_PROCESSES, MAX_FDS_PER_PROCESS};
use crate::filesystem::{BLOCK_SIZE, MAX_RECORD_SIZE, MAX_VIRTUAL_FILE_SIZE, READ_FILE_ALLOC_MAX_CONTENT};
use crate::memory::MAX_KERNEL_DMA_HEAP_ALLOC;
use crate::pipe::PIPE_BUF_CAP;
use crate::process::MAX_PROCESSES;
use crate::scheduler::MAX_CPUS;
use crate::scheme::SHM_REGION_MAX_BYTES;
use crate::servers::CONNECTION_BUFFER_CAP;

// --- DMA / DRM / heap ---

const _: () = assert!(MAX_GEM_BUFFER_SIZE == MAX_KERNEL_DMA_HEAP_ALLOC);
/// Un solo fichero virtual o TLV enorme no debe poder pedir más que el tope global de DMA/heap para un bloque.
const _: () = assert!(MAX_VIRTUAL_FILE_SIZE <= MAX_KERNEL_DMA_HEAP_ALLOC);
const _: () = assert!(MAX_RECORD_SIZE <= MAX_KERNEL_DMA_HEAP_ALLOC);
/// `read_file_alloc_inode` usa 32 MiB como tope de `Vec`; debe caber bajo el tope de registro TLV.
const _: () = assert!(READ_FILE_ALLOC_MAX_CONTENT <= MAX_RECORD_SIZE);

// --- Procesos / FD / scheduler SMP ---

const _: () = assert!(MAX_FD_PROCESSES == MAX_PROCESSES);
const _: () = assert!(MAX_FDS_PER_PROCESS > 0 && MAX_FDS_PER_PROCESS <= 256);
const _: () = assert!(MAX_CPUS == MAX_SMP_CPUS);
const _: () = assert!(MAX_CPUS >= 1 && MAX_CPUS <= 64);

// --- Red / pipes: buffers acotados respecto a la cola de conexión ---

const _: () = assert!(PIPE_BUF_CAP <= CONNECTION_BUFFER_CAP);

// --- Disco / FS: bloque lógico unificado ---

const _: () = assert!(BLOCK_SIZE == 4096);
const _: () = assert!(BLOCK_SIZE.is_power_of_two());

// --- IPC (constantes públicas en `ipc.rs`) ---

const _: () = assert!(crate::ipc::MAILBOX_DEPTH >= 16);
const _: () = assert!(crate::ipc::MAX_MESSAGE_DATA >= 64);
const _: () = assert!(crate::ipc::MAX_MESSAGE_DATA <= 4096);

// --- Virgl / DMA (tope fijo en `virtio::virgl_alloc_backing`) ---

const VIRGL_MAX_BACKING: usize = 16 * 1024 * 1024;
const _: () = assert!(VIRGL_MAX_BACKING <= MAX_KERNEL_DMA_HEAP_ALLOC);

/// Región SHM (creación / `ftruncate`) frente al tope global de una petición DMA/heap.
const _: () = assert!(SHM_REGION_MAX_BYTES <= MAX_KERNEL_DMA_HEAP_ALLOC);

/// `sys_ioctl` virgl submit (`MAX_SUBMIT_SIZE` en `syscalls.rs`).
const MAX_VIRGL_SUBMIT: usize = 256 * 1024;
const _: () = assert!(MAX_VIRGL_SUBMIT <= MAX_KERNEL_DMA_HEAP_ALLOC);

/// `VIRTIO_NET_MAX_TX_BYTES` en `virtio.rs` — debe caber en buffer de conexión de socket.
const VIRTIO_NET_TX_MAX: usize = 16 * 1024;
const _: () = assert!(VIRTIO_NET_TX_MAX <= CONNECTION_BUFFER_CAP);

/// `MAX_EXECVE_ARG_ENV_BYTES` — acotado frente al heap de kernel para argv+env acumulado.
const MAX_EXECVE_ARG_ENV: usize = 4 * 1024 * 1024;
const _: () = assert!(MAX_EXECVE_ARG_ENV <= MAX_KERNEL_DMA_HEAP_ALLOC);

/// `sys_read` y `read_file_alloc_inode` comparten el mismo orden de magnitud (32 MiB).
const SYS_READ_MAX_LEN: usize = 32 * 1024 * 1024;
const _: () = assert!(SYS_READ_MAX_LEN == READ_FILE_ALLOC_MAX_CONTENT);

/// Tabla de FD por proceso no supera el ancho de índice usado en rutas comunes (`< 256`).
const _: () = assert!(crate::fd::MAX_FDS_PER_PROCESS <= 256);
/// Menos FDs por proceso que «slots» de tabla global de procesos.
const _: () = assert!(crate::fd::MAX_FDS_PER_PROCESS <= MAX_FD_PROCESSES);

// --- ELF exec: el redondeo a 8 no debe colar un `Vec` de 128 MiB si el límite es < 128 MiB padded ---

const fn elf_byte_len_heap_padded(byte_len: u64) -> usize {
    let n = byte_len as usize;
    n.saturating_add(core::mem::size_of::<usize>() - 1) & !(core::mem::size_of::<usize>() - 1)
}

const ELF_HEAP_COPY_CAP: usize = 128 * 1024 * 1024;
const _: () = assert!(elf_byte_len_heap_padded(ELF_HEAP_COPY_CAP as u64 - 8) < ELF_HEAP_COPY_CAP);
const _: () = assert!(elf_byte_len_heap_padded(ELF_HEAP_COPY_CAP as u64 - 7) == ELF_HEAP_COPY_CAP);
