//! Constantes y lógica pura alineadas con `eclipse_kernel/`.
//! Cada símbolo indica en comentario el origen; si cambia el kernel, actualizar aquí y `cargo test`.

/// `eclipse_kernel/src/memory.rs` — `MAX_KERNEL_DMA_HEAP_ALLOC`
pub const MAX_KERNEL_DMA_HEAP_ALLOC: usize = 64 * 1024 * 1024;

/// `eclipse_kernel/src/drm.rs` — `MAX_GEM_BUFFER_SIZE` (= DMA heap cap)
pub const MAX_GEM_BUFFER_SIZE: usize = MAX_KERNEL_DMA_HEAP_ALLOC;

/// `eclipse_kernel/src/memory.rs` — `GPU_FW_MAX_SIZE`
pub const GPU_FW_MAX_SIZE: u64 = 32 * 1024 * 1024;

/// `eclipse_kernel/src/memory.rs` — `GPU_RPC_MAX_SIZE`
pub const GPU_RPC_MAX_SIZE: u64 = 1 * 1024 * 1024;

/// Misma fórmula que `virtio_display_pitch_and_size` en `eclipse_kernel/src/virtio.rs`.
pub fn virtio_bgra_framebuffer_bytes(width: u32, height: u32) -> Option<(usize, usize)> {
    if width == 0 || height == 0 {
        return None;
    }
    let pitch = usize::try_from(width.checked_mul(4)?).ok()?;
    let size_u64 = (pitch as u64).checked_mul(u64::from(height))?;
    if size_u64 > MAX_KERNEL_DMA_HEAP_ALLOC as u64 {
        return None;
    }
    let size = usize::try_from(size_u64).ok()?;
    Some((pitch, size))
}

/// `eclipse_kernel/src/syscalls.rs` — `MAX_EXECVE_ARG_ENV_BYTES`
pub const MAX_EXECVE_ARG_ENV_BYTES: usize = 4 * 1024 * 1024;

/// `eclipse_kernel/src/syscalls.rs` — submódulo `linux_mmap_abi` (mmap anónimo / flags).
pub const MMAP_PROT_MASK: u64 = 7;
pub const MMAP_PROT_EXEC: u64 = 4;
pub const MMAP_MAP_FIXED: u64 = 0x10;
pub const MMAP_MAP_SHARED: u64 = 0x01;
pub const MMAP_MAP_ANONYMOUS: u64 = 0x20;
pub const MMAP_MAP_POPULATE: u64 = 0x08000;
pub const MMAP_USER_ARENA_LO: u64 = 0x6000_0000;
pub const MMAP_USER_ARENA_HI: u64 = 0x7000_0000;
pub const MMAP_ANON_SLACK_BYTES: u64 = 0x8000;

/// `eclipse_kernel/src/virtio.rs` — `VIRTIO_NET_MAX_TX_BYTES`
pub const VIRTIO_NET_MAX_TX_BYTES: usize = 16 * 1024;

/// `eclipse_kernel/src/virtio.rs` — `virgl_alloc_backing`
pub const VIRGL_ALLOC_BACKING_MAX: usize = 16 * 1024 * 1024;

/// `eclipse_kernel/src/syscalls.rs` — `sys_read`
pub const SYS_READ_MAX_BYTES: u64 = 32 * 1024 * 1024;

/// `eclipse_kernel/src/syscalls.rs` — `sys_write`
pub const SYS_WRITE_MAX_BYTES: u64 = 1024 * 1024;

/// `eclipse_kernel/src/syscalls.rs` — `sys_get_logs`
pub const SYS_GET_LOGS_MAX_BYTES: u64 = 4096;

/// `eclipse_kernel/src/syscalls.rs` — `sys_receive`
pub const SYS_RECEIVE_MAX_BYTES: u64 = 4096;

/// `eclipse_kernel/src/syscalls.rs` — `sys_mmap` (límite superior de `length`)
pub const SYS_MMAP_MAX_LENGTH: u64 = 0x0000_7FFF_FFFF_FFFF;

/// `eclipse_kernel/src/syscalls.rs` — ioctl virgl submit
pub const MAX_SUBMIT_SIZE: usize = 256 * 1024;

/// `eclipse_kernel/src/servers.rs` — `CONNECTION_BUFFER_CAP`
pub const CONNECTION_BUFFER_CAP: usize = 256 * 1024;

/// `eclipse_kernel/src/syscalls.rs` — `MAX_PASS_FDS` (sendmsg SCM_RIGHTS)
pub const MAX_PASS_FDS: usize = 8;

/// `eclipse_kernel/src/filesystem.rs`
pub const BLOCK_SIZE: usize = 4096;
pub const MAX_RECORD_SIZE: usize = 32 * 1024 * 1024;
pub const MAX_VIRTUAL_FILE_SIZE: usize = 64 * 1024 * 1024;

/// `eclipse_kernel/src/filesystem.rs` — `READ_FILE_ALLOC_MAX_CONTENT` / `read_file_alloc_inode`
pub const READ_FILE_ALLOC_MAX_CONTENT: usize = 32 * 1024 * 1024;
/// Alias usado por tests más antiguos (`MAX_WHOLE_FILE_READ` en comentarios del kernel).
pub const MAX_WHOLE_FILE_READ: usize = READ_FILE_ALLOC_MAX_CONTENT;

/// `eclipse_kernel/src/filesystem.rs` — `read_file_alloc_follow`
pub const MAX_SYMLINK_DEPTH: usize = 16;

/// `eclipse_kernel/src/ipc.rs` — `MAILBOX_DEPTH` / `MAX_MESSAGE_DATA`
pub const MAILBOX_DEPTH: usize = 256;
pub const MAX_MESSAGE_DATA: usize = 512;

/// `eclipse_kernel/src/scheme.rs` — `SHM_REGION_MAX_BYTES` (creación SHM y `ftruncate`)
pub const SHM_REGION_MAX_BYTES: usize = 16 * 1024 * 1024;
/// Alias histórico en tests (`ShmScheme::ftruncate` usaba el mismo valor).
pub const SHM_FTRUNCATE_MAX: usize = SHM_REGION_MAX_BYTES;

/// `eclipse_kernel/src/syscalls.rs` — `MAX_PATH_LENGTH` (compromiso frente a PATH_MAX 4096)
pub const MAX_PATH_LENGTH: usize = 1024;

/// `eclipse_kernel/src/syscalls.rs` — tope usado en `strlen_user_unique(path_ptr, …)` y validación de rutas
pub const SYSCALL_PATH_STRLEN_CAP: usize = 4096;

/// `eclipse_kernel/src/syscalls.rs` — `sys_send` / buffer interno de mensaje
pub const SYS_SEND_MAX_MSG: usize = 512;

/// `eclipse_kernel/src/syscalls.rs` — `sys_ioctl` backend NVIDIA (`MAX_PAYLOAD`)
pub const NVIDIA_IOCTL_MAX_PAYLOAD: usize = 64;

/// `eclipse_kernel/src/syscalls.rs` — `CMSG_HDR_SIZE` (sendmsg ancillary)
pub const CMSG_HDR_SIZE: u64 = 16;

/// `eclipse_kernel/src/syscalls.rs` — `USER_STACK_SIZE` (spawn/exec)
pub const USER_STACK_SIZE: usize = 0x10_0000;

/// `eclipse_kernel/src/fd.rs`
pub const MAX_FDS_PER_PROCESS: usize = 64;
pub const MAX_FD_PROCESSES: usize = 256;

/// `eclipse_kernel/src/process.rs`
pub const KERNEL_STACK_SIZE: usize = 32768;
pub const MAX_PROCESSES: usize = 256;
pub const PROCESS_MAX_CPUS: usize = 32;

/// `eclipse_kernel/src/boot.rs` — `MAX_SMP_CPUS`
pub const MAX_SMP_CPUS: usize = 32;

/// `eclipse_kernel/src/boot.rs` — `DF_STACK_SIZE`
pub const DF_STACK_SIZE: usize = 8192;

/// `eclipse_kernel/src/scheduler.rs` — `MAX_PIDS` / `SLEEP_QUEUE_SIZE` / `MAX_CPUS`
pub const MAX_PIDS: usize = 256;
pub const SLEEP_QUEUE_SIZE: usize = 256;
pub const SCHEDULER_MAX_CPUS: usize = 32;

/// `eclipse_kernel/src/memory.rs` — `PageTable::entries`
pub const PAGE_TABLE_ENTRIES: usize = 512;

/// `eclipse_kernel/src/memory.rs` — región imagen kernel (enlace)
pub const KERNEL_REGION_SIZE: u64 = 0x8000_0000;

/// `eclipse_kernel/src/ipc.rs` — `Server::message_queue` longitud
pub const SERVER_MESSAGE_QUEUE_LEN: usize = 64;

/// `eclipse_kernel/src/ipc.rs` — `PID_MAP_SIZE`
pub const PID_MAP_SIZE: usize = 4096;

/// `eclipse_kernel/src/interrupts.rs`
pub const KEY_BUFFER_SIZE: usize = 256;
pub const MOUSE_BUFFER_SIZE: usize = 128;

/// `eclipse_kernel/src/pipe.rs` — `PIPE_BUF_CAP` (pública en el kernel para `invariants`)
pub const PIPE_BUF_CAP: usize = 65536;

/// `eclipse_kernel/src/filesystem.rs` — caches
pub const INODE_CACHE_SIZE: usize = 128;
pub const DIR_CACHE_SIZE: usize = 32;

/// `eclipse_kernel/src/progress.rs` — buffer de líneas / truncado HUD
pub const LOG_BUF_SIZE: usize = 128;
pub const LOG_CHAR_LIMIT: usize = 64;

/// `eclipse_kernel/src/servers.rs` — tamaño de evento en cola de input
pub const INPUT_EVENT_SIZE: usize = 24;

/// `eclipse_kernel/src/servers.rs` — `MAX_QUEUE_BYTES` (= `CONNECTION_BUFFER_CAP`)
pub const MAX_QUEUE_BYTES: usize = 256 * 1024;

/// `eclipse_kernel/src/elf_loader.rs` — `MAX_PROCESS_NAME_LEN`
pub const MAX_PROCESS_NAME_LEN: usize = 16;

/// `eclipse_kernel/src/bcache.rs` — `CACHE_SIZE`
pub const BCACHE_CACHE_SIZE: usize = 1024;

/// `eclipse_kernel/src/usb_hid.rs` — `XhciControllerState::hid_devices` (slots por controlador)
pub const XHCI_HID_ENDPOINT_SLOTS: usize = 8;

/// `eclipse_kernel/src/e1000e.rs`
pub const E1000E_RX_RING_SIZE: usize = 128;
pub const E1000E_TX_RING_SIZE: usize = 128;
pub const E1000E_PACKET_BUF_SIZE: usize = 2048;

/// Copia de `elf_byte_len_heap_padded` / `elf_size_allowed_for_kernel_heap_copy` en `eclipse_kernel/src/syscalls.rs`.
#[inline]
pub fn elf_byte_len_heap_padded(byte_len: u64) -> usize {
    let n = byte_len as usize;
    n.saturating_add(std::mem::size_of::<usize>() - 1) & !(std::mem::size_of::<usize>() - 1)
}

#[inline]
pub fn elf_size_allowed_for_kernel_heap_copy(byte_len: u64) -> bool {
    byte_len > 0 && elf_byte_len_heap_padded(byte_len) < 128 * 1024 * 1024
}

/// `true` si `read_file_alloc_inode` rechazaría por tamaño (equivale a `Err("File too large")` para len>0).
#[inline]
pub fn read_file_inode_too_large_for_heap(len: usize) -> bool {
    const ALLOC_ALIGN: usize = std::mem::size_of::<usize>();
    len.saturating_add(ALLOC_ALIGN - 1) >= READ_FILE_ALLOC_MAX_CONTENT
}

/// Igual que `data.len().min(MAX_MESSAGE_DATA)` en `eclipse_kernel/src/ipc.rs` (`send_message`).
#[inline]
pub fn ipc_clip_payload(len: usize) -> usize {
    len.min(MAX_MESSAGE_DATA)
}

/// Misma fórmula que `ReentrantMutex::{pack,unpack}` en `eclipse_kernel/src/sync.rs`.
#[inline]
pub fn reentrant_mutex_pack(owner: i32, depth: u32) -> u64 {
    ((owner as u32 as u64) << 32) | (depth as u64)
}

#[inline]
pub fn reentrant_mutex_unpack(state: u64) -> (i32, u32) {
    ((state >> 32) as i32, (state & 0xFFFF_FFFF) as u32)
}

/// Tamaño del *Input Context* para Configure Endpoint (XHCI): 33 × context entry size.
#[inline]
pub fn xhci_configure_endpoint_input_context_bytes(context_entry_bytes: usize) -> usize {
    33 * context_entry_bytes
}

/// Desplazamiento del *Endpoint Context* `ep_id` (1-based) dentro del input context.
#[inline]
pub fn xhci_endpoint_context_offset(context_entry_bytes: usize, ep_id: usize) -> usize {
    debug_assert!(ep_id >= 1);
    2 * context_entry_bytes + (ep_id - 1) * context_entry_bytes
}

/// `eclipse_kernel/src/progress.rs` — truncado de línea larga para el buffer de logs
#[inline]
pub fn progress_truncate_line_for_log(line: &str) -> &str {
    if line.len() > LOG_CHAR_LIMIT {
        &line[line.len() - LOG_CHAR_LIMIT..]
    } else {
        line
    }
}

/// Modelo FIFO del ring `ProcessMailbox` en `eclipse_kernel/src/ipc.rs` (misma aritmética, sin `Message`).
#[derive(Debug)]
pub struct RingMailbox {
    cap: usize,
    slots: Vec<u32>,
    head: usize,
    tail: usize,
    len: usize,
}

impl RingMailbox {
    pub fn new(capacity: usize) -> Self {
        Self {
            cap: capacity,
            slots: vec![0; capacity],
            head: 0,
            tail: 0,
            len: 0,
        }
    }

    pub fn push(&mut self, v: u32) -> bool {
        if self.len >= self.cap {
            return false;
        }
        self.slots[self.tail] = v;
        self.tail = (self.tail + 1) % self.cap;
        self.len += 1;
        true
    }

    pub fn pop(&mut self) -> Option<u32> {
        if self.len == 0 {
            return None;
        }
        let v = self.slots[self.head];
        self.head = (self.head + 1) % self.cap;
        self.len -= 1;
        Some(v)
    }

    pub fn len(&self) -> usize {
        self.len
    }
}
