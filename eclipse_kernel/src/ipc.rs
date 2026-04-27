//! Sistema IPC (Inter-Process Communication) del microkernel
//! 
//! Implementa comunicación por mensajes entre procesos y servidores

use spin::Mutex;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use crate::process::MAX_PROCESSES;

/// Mensajes Input/Signal descartados por mailbox lleno (debug: si >0 al congelarse input, aumentar MAILBOX_DEPTH o drenar más).
pub(crate) static DROPPED_P2P_MSGS: AtomicU64 = AtomicU64::new(0);
/// Mensajes P2P entregados a un mailbox (reseteado en heartbeat; si recv_ok=0 pero esto >0, se entrega a otro slot).
pub(crate) static P2P_DELIVERED: AtomicU64 = AtomicU64::new(0);

/// ID de mensaje
pub type MessageId = u64;

/// ID de servidor
pub type ServerId = u32;

/// ID de cliente
pub type ClientId = u32;

/// Tamaño máximo de datos en un mensaje
pub const MAX_MESSAGE_DATA: usize = 512;

/// Tipos de mensaje
#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum MessageType {
    System = 0x00000001,
    Memory = 0x00000002,
    FileSystem = 0x00000004,
    Network = 0x00000008,
    Graphics = 0x00000010,
    Audio = 0x00000020,
    Input = 0x00000040,
    AI = 0x00000080,
    Security = 0x00000100,
    User = 0x00000200,
    Signal = 0x00000400, // Process-to-Process signal (not for Kernel Servers)
    /// Línea de log del kernel enviada al HUD (compositor/smithay) cuando el logo ya está dibujado.
    Log = 0x00000800,
}

impl MessageType {
    pub fn from_u32(val: u32) -> Self {
        match val {
            0x00000001 => MessageType::System,
            0x00000002 => MessageType::Memory,
            0x00000004 => MessageType::FileSystem,
            0x00000008 => MessageType::Network,
            0x00000010 => MessageType::Graphics,
            0x00000020 => MessageType::Audio,
            0x00000040 => MessageType::Input,
            0x00000080 => MessageType::AI,
            0x00000100 => MessageType::Security,
            0x00000200 => MessageType::User,
            0x00000400 => MessageType::Signal,
            0x00000800 => MessageType::Log,
            _          => MessageType::User,
        }
    }
    pub fn to_u32(self) -> u32 {
        self as u32
    }
}

/// Mensaje del microkernel
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Message {
    pub id: MessageId,
    pub from: ClientId,
    pub to: ServerId,
    pub msg_type: MessageType,
    pub data: [u8; MAX_MESSAGE_DATA],
    pub data_size: u32,
    pub priority: u8,
    pub flags: u8,
    /// Îndice de slot en PROCESS_TABLE del destinatario (0-63), calculado en send_message.
    /// Permite delivery O(1) sin buscar en PROCESS_TABLE ni PROCESS_MAILBOXES.
    pub dest_slot: u8,
}

impl Message {
    pub const fn new() -> Self {
        Self {
            id: 0,
            from: 0,
            to: 0,
            msg_type: MessageType::System,
            data: [0; MAX_MESSAGE_DATA],
            data_size: 0,
            priority: 0,
            flags: 0,
            dest_slot: 0xFF, // 0xFF = desconocido / usar slow path
        }
    }
}

/// Estado de servidor
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ServerState {
    Inactive,
    Starting,
    Active,
    Paused,
    Terminating,
}

/// Servidor del sistema
#[derive(Clone, Copy)]
pub struct Server {
    pub id: ServerId,
    pub name: [u8; 32],
    pub msg_type: MessageType,
    pub priority: u8,
    pub state: ServerState,
    pub message_queue: [Option<Message>; 64],
    pub queue_head: usize,
    pub queue_tail: usize,
    pub messages_processed: u64,
    /// Número de mensajes descartados porque la cola estaba llena
    pub dropped_messages: u64,
}

impl Server {
    const fn new() -> Self {
        const NONE_MESSAGE: Option<Message> = None;
        Self {
            id: 0,
            name: [0; 32],
            msg_type: MessageType::System,
            priority: 0,
            state: ServerState::Inactive,
            message_queue: [NONE_MESSAGE; 64],
            queue_head: 0,
            queue_tail: 0,
            messages_processed: 0,
            dropped_messages: 0,
        }
    }
}

/// Cliente del sistema
#[derive(Clone, Copy)]
pub struct Client {
    pub id: ClientId,
    pub name: [u8; 32],
    pub server_id: ServerId,
    pub permissions: u32,
    pub messages_sent: u64,
}

impl Client {
    const fn new() -> Self {
        Self {
            id: 0,
            name: [0; 32],
            server_id: 0,
            permissions: 0,
            messages_sent: 0,
        }
    }
}

struct IpcSystem {
    servers: [Option<Server>; 32],
    clients: [Option<Client>; 256],
    message_id_counter: AtomicU32,
    server_id_counter: AtomicU32,
    client_id_counter: AtomicU32,
    global_message_queue: [Option<Message>; 1024],
    total_messages: u64,
}

static GLOBAL_QUEUE_HEAD: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static GLOBAL_QUEUE_TAIL: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

impl IpcSystem {
    const fn new() -> Self {
        const NONE_SERVER: Option<Server> = None;
        const NONE_CLIENT: Option<Client> = None;
        const NONE_MESSAGE: Option<Message> = None;
        
        Self {
            servers: [NONE_SERVER; 32],
            clients: [NONE_CLIENT; 256],
            message_id_counter: AtomicU32::new(1),
            server_id_counter: AtomicU32::new(1),
            client_id_counter: AtomicU32::new(1),
            global_message_queue: [NONE_MESSAGE; 1024],
            total_messages: 0,
        }
    }
}

// Sin alloc: los mailboxes usan ring buffers estáticos

static IPC_SYSTEM: Mutex<IpcSystem> = Mutex::new(IpcSystem::new());

/// **Tabla inversa PID → slot index (O(1)).**
///
/// Each entry stores the full (pid, slot) pair so that stale entries from
/// recycled PIDs are distinguishable.  When a new process is created with
/// the same `pid % PID_MAP_SIZE` as an older (now terminated) PID, the
/// old entry is simply overwritten.  A lookup first confirms that the
/// stored `pid` field matches the requested PID before trusting the slot.
///
/// Without the pid field a collision (PID A and PID B both hash to the
/// same index) would silently route IPC messages for PID A to whatever
/// slot PID B registered — a silent data corruption bug that manifests
/// after ~256 process lifecycle events.
// 4096 entradas: PIDs secuenciales < 4096 nunca colisionan (pid % 4096 == pid).
// Subir este valor si en el futuro los PIDs superan 4096.
const PID_MAP_SIZE: usize = 4096;

#[derive(Clone, Copy)]
struct PidSlotEntry {
    pid: u32,
    slot: u8,
}

const EMPTY_PSE: PidSlotEntry = PidSlotEntry { pid: u32::MAX, slot: 0xFF };
static PID_SLOT_MAP: Mutex<[PidSlotEntry; PID_MAP_SIZE]> = Mutex::new([EMPTY_PSE; PID_MAP_SIZE]);

/// Registrar un PID en la tabla inversa al crear un proceso.
/// Llamar desde `create_process_with_pid` al insertar en PROCESS_TABLE.
pub fn register_pid_slot(pid: crate::process::ProcessId, slot: usize) {
    let idx = pid as usize % PID_MAP_SIZE;
    crate::serial::serial_printf(format_args!("[debug] register_pid_slot pid={} slot={} idx={}\n", pid, slot, idx));
    run_critical(|| {
        let mut map = PID_SLOT_MAP.lock();
        crate::serial::serial_print("[debug] register_pid_slot: lock acquired\n");
        map[idx] = PidSlotEntry { pid, slot: slot as u8 };
    });
    crate::serial::serial_print("[debug] register_pid_slot: done\n");
}

/// Eliminar un PID de la tabla inversa al terminar un proceso.
/// Llamar desde `exit_process` antes de limpiar el mailbox.
pub fn unregister_pid_slot(pid: crate::process::ProcessId) {
    let idx = pid as usize % PID_MAP_SIZE;
    run_critical(|| {
        let mut map = PID_SLOT_MAP.lock();
        // Only clear if this exact PID owns the entry; a newer PID may have
        // already claimed the same hash bucket (hash collision / PID reuse).
        if map[idx].pid == pid {
            map[idx] = EMPTY_PSE;
        }
    });
}

/// Lookup O(1): PID → slot index via the inverse map.
///
/// Con PID_MAP_SIZE = 4096 y PIDs secuenciales pequeños, `pid % 4096 == pid`
/// → nunca hay colisión. NO hace fallback a PROCESS_TABLE (que puede estar
/// bloqueada por el scheduler/spawn y provocar deadlock de spinlock).
pub fn pid_to_slot_fast(pid: crate::process::ProcessId) -> Option<usize> {
    let idx = pid as usize % PID_MAP_SIZE;
    let entry = run_critical(|| {
        PID_SLOT_MAP.lock()[idx]
    });
    if entry.pid == pid && entry.slot != 0xFF {
        Some(entry.slot as usize)
    } else {
        None
    }
}

/// **Ring buffer estático por proceso — SIN heap, seguro en IRQs.**
/// Capacidad por proceso: picos de input (ratón/teclado) cuando el compositor está ocupado.
/// Si se llena, se descartan eventos → ratón/teclado "bloqueados". 256 da margen amplio.
/// 256 × ~256 bytes ≈ 64 KB por slot.
pub const MAILBOX_DEPTH: usize = 256;
struct ProcessMailbox {
    msgs: [Message; MAILBOX_DEPTH],
    head: usize,
    tail: usize,
    len:  usize,
}
impl ProcessMailbox {
    const fn new() -> Self {
        const EMPTY: Message = Message::new();
        Self { msgs: [EMPTY; MAILBOX_DEPTH], head: 0, tail: 0, len: 0 }
    }
    #[inline(always)]
    fn check_invariants(&self) {
        if self.len > MAILBOX_DEPTH || self.head >= MAILBOX_DEPTH || self.tail >= MAILBOX_DEPTH {
            crate::serial::serial_printf(format_args!(
                "[IPC-MB] CRITICAL: Invariant violation! len={} head={} tail={} depth={}\n",
                self.len, self.head, self.tail, MAILBOX_DEPTH
            ));
            // In a production kernel we might panic, but here we'll try to survive by resetting.
            // panic!("IPC Mailbox corruption detected");
        }
    }
    fn push(&mut self, msg: Message) -> bool {
        self.check_invariants();
        if self.len >= MAILBOX_DEPTH { return false; }
        self.msgs[self.tail] = msg;
        self.tail = (self.tail + 1) % MAILBOX_DEPTH;
        self.len += 1;
        true
    }
    fn pop(&mut self) -> Option<Message> {
        self.check_invariants();
        if self.len == 0 { return None; }
        let msg = self.msgs[self.head];
        self.head = (self.head + 1) % MAILBOX_DEPTH;
        self.len -= 1;
        Some(msg)
    }
    /// Extrae el primer mensaje que cumpla el predicado, sin perder el resto.
    /// Útil para syscalls síncronos que esperan un ID específico.
    fn pop_filtered<F>(&mut self, filter: F) -> Option<Message>
    where F: Fn(&Message) -> bool {
        self.check_invariants();
        if self.len == 0 { return None; }
        
        let mut current = self.head;
        let original_len = self.len;
        
        for i in 0..original_len {
            // Safety: current is always wrapped by MAILBOX_DEPTH.
            if filter(&self.msgs[current]) {
                let msg = self.msgs[current];
                
                // Shift subsequent messages back to fill the gap.
                // Number of elements to shift is (len - 1) - i.
                let steps = (self.len - 1) - i;
                let mut move_idx = current;
                
                for _ in 0..steps {
                    let next = (move_idx + 1) % MAILBOX_DEPTH;
                    self.msgs[move_idx] = self.msgs[next];
                    move_idx = next;
                }
                
                // Update tail and len
                self.tail = (self.tail + MAILBOX_DEPTH - 1) % MAILBOX_DEPTH;
                self.len -= 1;
                return Some(msg);
            }
            current = (current + 1) % MAILBOX_DEPTH;
        }
        None
    }
    fn peek(&self) -> Option<&Message> {
        if self.len == 0 { None } else { Some(&self.msgs[self.head]) }
    }
    fn clear(&mut self) {
        self.head = 0; self.tail = 0; self.len = 0;
    }
}

/// Buzones de mensajes por proceso — ring buffers estáticos, sin heap.
/// Indexados por SLOT INDEX en PROCESS_TABLE (0-255).
const EMPTY_MAILBOX: ProcessMailbox = ProcessMailbox::new();
static PROCESS_MAILBOXES: Mutex<[ProcessMailbox; MAX_PROCESSES]> =
    Mutex::new([EMPTY_MAILBOX; MAX_PROCESSES]);

/// Helper to run a closure with interrupts disabled.
/// In tests, it just runs the closure to avoid x86_64 instruction dependency.
fn run_critical<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    #[cfg(not(test))]
    {
        x86_64::instructions::interrupts::without_interrupts(f)
    }
    #[cfg(test)]
    {
        f()
    }
}

/// Inicializar el sistema IPC
pub fn init() {
    run_critical(|| {
        let mut ipc = IPC_SYSTEM.lock();
        ipc.message_id_counter.store(1, Ordering::SeqCst);
        ipc.server_id_counter.store(1, Ordering::SeqCst);
        ipc.client_id_counter.store(1, Ordering::SeqCst);
        GLOBAL_QUEUE_HEAD.store(0, Ordering::SeqCst);
        GLOBAL_QUEUE_TAIL.store(0, Ordering::SeqCst);
        ipc.total_messages = 0;
    });
    // Reset mailboxes por separado (sin IPC_SYSTEM)
    run_critical(|| {
        let mut mailboxes = PROCESS_MAILBOXES.lock();
        for mb in mailboxes.iter_mut() { mb.clear(); }
    });
}

/// Registrar un servidor
pub fn register_server(name: &[u8], msg_type: MessageType, priority: u8) -> Option<ServerId> {
    run_critical(|| {
        let mut ipc = IPC_SYSTEM.lock();
        let server_id = ipc.server_id_counter.fetch_add(1, Ordering::SeqCst);
        
        // Buscar slot libre
        for i in 0..32 {
            if ipc.servers[i].is_none() {
                let mut server = Server::new();
                server.id = server_id;
                server.msg_type = msg_type;
                server.priority = priority;
                server.state = ServerState::Starting;
                
                // Copiar nombre
                let name_len = core::cmp::min(name.len(), 31);
                for j in 0..name_len {
                    server.name[j] = name[j];
                }
                
                ipc.servers[i] = Some(server);
                return Some(server_id);
            }
        }
        
        None
    })
}

/// Registrar un servidor
/// Los mensajes pendientes en su cola se descartan.
/// Devuelve true si el servidor fue encontrado y eliminado.
pub fn unregister_server(server_id: ServerId) -> bool {
    run_critical(|| {
        let mut ipc = IPC_SYSTEM.lock();
        for i in 0..32 {
            if let Some(ref server) = ipc.servers[i] {
                if server.id == server_id {
                    ipc.servers[i] = None;
                    return true;
                }
            }
        }
        false
    })
}

/// Registrar un cliente
pub fn register_client(name: &[u8], server_id: ServerId, permissions: u32) -> Option<ClientId> {
    run_critical(|| {
        let mut ipc = IPC_SYSTEM.lock();
        let client_id = ipc.client_id_counter.fetch_add(1, Ordering::SeqCst);
        
        // Buscar slot libre
        for i in 0..MAX_PROCESSES {
            if ipc.clients[i].is_none() {
                let mut client = Client::new();
                client.id = client_id;
                client.server_id = server_id;
                client.permissions = permissions;
                
                // Copiar nombre
                let name_len = core::cmp::min(name.len(), 31);
                for j in 0..name_len {
                    client.name[j] = name[j];
                }
                
                ipc.clients[i] = Some(client);
                return Some(client_id);
            }
        }
        
        None
    })
}

/// Enviar un mensaje.
/// Para mensajes P2P (Input/Signal) con dest_slot válido: **direct delivery** al
/// ProcessMailbox del proceso destino, sin pasar por la cola global.
/// Para mensajes de servidor (Graphics, System, etc.): cola global como antes.
pub fn send_message(from: ClientId, to: ServerId, msg_type: MessageType, data: &[u8]) -> bool {
    // O(1): precalcular slot antes de cualquier lock.
    // Ahora intentamos entrega P2P para CUALQUIER tipo de mensaje si el destino es un PID válido.
    // Esto permite que procesos como 'init' reciban mensajes de sistema/usuario sin ser servidores registrados.
    let mut dest_slot = pid_to_slot_fast(to).map(|s| s as u8).unwrap_or(0xFF);
    // Importante: si el mapa O(1) falla (colisión / PID aún no registrado en la tabla rápida),
    // y el mensaje es P2P (Input/Signal), NO podemos caer a la cola global: `receive()` del
    // proceso solo drena PROCESS_MAILBOXES, y la cola global requiere `process_messages()`.
    // Hacemos fallback a la búsqueda lenta en la tabla de procesos.
    if dest_slot == 0xFF && (msg_type == MessageType::Signal || msg_type == MessageType::Input) {
        if let Some(s) = crate::process::pid_to_slot(to) {
            dest_slot = s as u8;
        }
    }

    // Construir mensaje (sin ningún lock aún)
    let mut msg = Message::new();
    msg.from = from;
    msg.to = to;
    msg.msg_type = msg_type;
    msg.dest_slot = dest_slot;
    let data_len = data.len().min(MAX_MESSAGE_DATA);
    msg.data[..data_len].copy_from_slice(&data[..data_len]);
    msg.data_size = data_len as u32;

    // --- Direct delivery para P2P / Process Mailbox: bypass de cola global ---
    // Si conocemos el slot del proceso, enviamos directamente a su buzón.
    //
    // IMPORTANTE: NO re-verificamos el slot dentro de run_critical llamando a
    // pid_to_slot() porque eso intenta bloquear PROCESS_TABLE, que puede estar
    // ya bloqueada por spawn/fork → deadlock de spinlock en single-CPU.
    // dest_slot fue calculado arriba de forma segura (fuera de cualquier lock
    // de PROCESS_TABLE), así que lo usamos directamente.
    if dest_slot != 0xFF {
        #[cfg(not(test))]
        if to == 1 {
            crate::serial::serial_printf(format_args!(
                "[IPC-TRACE] send_message ->PID1 from={} type={:?} dest_slot={} len={}\n",
                from, msg_type, dest_slot, data_len
            ));
        }
        return run_critical(|| {
            static P2P_ID: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);
            let mut m = msg;
            m.id = P2P_ID.fetch_add(1, Ordering::Relaxed);
            m.dest_slot = dest_slot;
            #[cfg(not(test))]
            if to == 1 {
                crate::serial::serial_printf(format_args!(
                    "[IPC-TRACE] send_message ->PID1 acquiring PROCESS_MAILBOXES lock...\n"
                ));
            }
            let mut mailboxes = PROCESS_MAILBOXES.lock();
            #[cfg(not(test))]
            if to == 1 {
                crate::serial::serial_printf(format_args!(
                    "[IPC-TRACE] send_message ->PID1 got PROCESS_MAILBOXES lock\n"
                ));
            }
            let ok = mailboxes[dest_slot as usize].push(m);
            if ok {
                P2P_DELIVERED.fetch_add(1, Ordering::Relaxed);
            } else {
                DROPPED_P2P_MSGS.fetch_add(1, Ordering::Relaxed);
                #[cfg(not(test))]
                if to == 1 {
                    crate::serial::serial_printf(format_args!(
                        "[IPC-TRACE] send_message ->PID1 mailbox FULL (slot={})\n",
                        dest_slot
                    ));
                }
            }
            ok
        });
    }

    // --- Cola global para mensajes de servidor (o fallback P2P si dest_slot=0xFF) ---
    if dest_slot == 0xFF && (msg_type == MessageType::Signal || msg_type == MessageType::Input) {
        #[cfg(not(test))]
        crate::serial::serial_printf(format_args!(
            "[IPC] P2P msg type {:?} for PID {} fell back to global queue\n",
            msg_type, to
        ));
    }

    run_critical(|| {
        let mut ipc = IPC_SYSTEM.lock();
        msg.id = ipc.message_id_counter.fetch_add(1, Ordering::SeqCst) as u64;
        let tail = GLOBAL_QUEUE_TAIL.load(Ordering::Relaxed);
        let next_tail = (tail + 1) % 1024;
        if next_tail == GLOBAL_QUEUE_HEAD.load(Ordering::Relaxed) { return false; }
        ipc.global_message_queue[tail] = Some(msg);
        GLOBAL_QUEUE_TAIL.store(next_tail, Ordering::Release);
        ipc.total_messages += 1;
        true
    })
}

/// Procesar mensajes pendientes
pub fn process_messages() {
    // CRITICAL: IPC_SYSTEM and PROCESS_MAILBOXES must NEVER be held simultaneously.
    // On multi-core systems both locks can be held by different CPUs with IRQs disabled,
    // causing the mouse IRQ to be delayed long enough to lose PS/2 bytes.
    // Fix: extract each message under IPC_SYSTEM alone, release it, then deliver
    // P2P messages to the mailbox under PROCESS_MAILBOXES alone.
    let pending = {
        let head = GLOBAL_QUEUE_HEAD.load(Ordering::Relaxed);
        let tail = GLOBAL_QUEUE_TAIL.load(Ordering::Relaxed);
        if head == tail {
            0
        } else if tail > head {
            tail - head
        } else {
            1024 - head + tail
        }
    };

    for _ in 0..pending {
        // Phase 1: Extract one message from the global queue (IPC_SYSTEM only).
        // Returns: Some(Some((pid, msg))) = P2P for mailbox delivery
        //          Some(None)             = message delivered to server (or dropped)
        //          None                   = queue empty, stop
        let p2p = run_critical(|| {
            let mut ipc = IPC_SYSTEM.lock();

            let head = GLOBAL_QUEUE_HEAD.load(Ordering::Acquire);
            if head == GLOBAL_QUEUE_TAIL.load(Ordering::Relaxed) {
                return None; // Queue empty - stop the outer loop
            }

            if let Some(msg) = ipc.global_message_queue[head] {
                // Signal and Input (P2P): route to Process Mailbox
                let is_p2p = msg.msg_type == MessageType::Signal
                    || msg.msg_type == MessageType::Input;
                if is_p2p {
                    let mut slot = msg.dest_slot; // O(1): ya calculado en send_message
                    
                    // Fallback para P2P que cayó a cola global (colisión o PID aún no registrado)
                    if slot == 0xFF {
                        if let Some(s) = crate::process::pid_to_slot(msg.to) {
                            slot = s as u8;
                        }
                    }

                    ipc.global_message_queue[head] = None;
                    GLOBAL_QUEUE_HEAD.store((head + 1) % 1024, Ordering::Release);
                    if slot != 0xFF {
                        return Some(Some((slot as usize, msg))); // deliver to mailbox in Phase 2
                    }
                    // dest_slot inválido: proceso ya terminó → descartar
                    return Some(None);
                }

                // Non-P2P: deliver to a registered kernel server
                let mut found_server_idx = None;
                for i in 0..32 {
                    if let Some(ref server) = ipc.servers[i] {
                        if server.id == msg.to {
                            found_server_idx = Some(i);
                            break;
                        }
                    }
                }
                if let Some(idx) = found_server_idx {
                    if let Some(taken_msg) = ipc.global_message_queue[head].take() {
                        if let Some(ref mut server) = ipc.servers[idx] {
                            let tail = server.queue_tail;
                            let next_tail = (tail + 1) % 64;
                            if next_tail != server.queue_head {
                                server.message_queue[tail] = Some(taken_msg);
                                server.queue_tail = next_tail;
                                server.messages_processed += 1;
                            } else {
                                // Cola del servidor llena: descartar y contabilizar
                                server.dropped_messages += 1;
                                #[cfg(not(test))]
                                crate::serial::serial_printf(format_args!(
                                    "[IPC] Server {} queue full, dropped msg (total: {})\n",
                                    server.id, server.dropped_messages
                                ));
                            }
                        }
                    }
                } else {
                    // No server found – clear slot so it isn't processed again
                    ipc.global_message_queue[head] = None;
                }
                GLOBAL_QUEUE_HEAD.store((head + 1) % 1024, Ordering::Release);
            } else {
                GLOBAL_QUEUE_HEAD.store((head + 1) % 1024, Ordering::Release);
            }
            Some(None) // continue outer loop
        });

        match p2p {
            None => break,
            Some(None) => {} // server delivery or dropped
            Some(Some((slot, msg))) => {
                // Phase 2: Deliver to process mailbox (PROCESS_MAILBOXES only).
                let ok = PROCESS_MAILBOXES.lock()[slot].push(msg);
                if ok {
                    P2P_DELIVERED.fetch_add(1, Ordering::Relaxed);
                } else {
                    DROPPED_P2P_MSGS.fetch_add(1, Ordering::Relaxed);
                    #[cfg(not(test))]
                    crate::serial::serial_printf(format_args!(
                        "[IPC] P2P Mailbox {} full, dropped msg type {:?}\n",
                        slot, msg.msg_type
                    ));
                }
            }
        }
    }
}

/// ¿Hay mensajes pendientes por procesar?
pub fn has_pending_messages() -> bool {
    GLOBAL_QUEUE_HEAD.load(Ordering::Relaxed) != GLOBAL_QUEUE_TAIL.load(Ordering::Relaxed)
}

/// Obtener estadísticas del sistema IPC
pub fn get_stats() -> (u32, u32, u64) {
    run_critical(|| {
        let ipc = IPC_SYSTEM.lock();
        let mut active_servers = 0;
        let mut active_clients = 0;
        
        for server in &ipc.servers {
            if let Some(_) = server {
                active_servers += 1;
            }
        }
        
        for client in &ipc.clients {
            if let Some(_) = client {
                active_clients += 1;
            }
        }
        
        (active_servers, active_clients, ipc.total_messages)
    })
}

static LAST_IPC_LOG: AtomicU64 = AtomicU64::new(0);

/// Emitir un log de estado del IPC cada ~60 s (heartbeat) para no inundar el serial.
/// Llamado desde el bucle idle de los núcleos.
pub fn p2p_heartbeat() {
    let now = crate::interrupts::ticks();
    let last = LAST_IPC_LOG.load(Ordering::Relaxed);
    if now >= last + 60000 {
        if LAST_IPC_LOG.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
            let delivered = P2P_DELIVERED.load(Ordering::Relaxed);
            let dropped = DROPPED_P2P_MSGS.load(Ordering::Relaxed);
            let (sv, cl, total) = get_stats();
            crate::serial::serial_printf(format_args!(
                "[IPC] Heartbeat: Delivered P2P: {}, Dropped P2P: {}, Total Msgs: {}, Sv: {}, Cl: {}\n",
                delivered, dropped, total, sv, cl
            ));
            audit_mailboxes();
        }
    }
}

/// Escanea todos los buzones en busca de inconsistencias (debug).
pub fn audit_mailboxes() {
    run_critical(|| {
        let mut mailboxes = PROCESS_MAILBOXES.lock();
        for i in 0..MAX_PROCESSES {
            let mb = &mut mailboxes[i];
            if mb.len > MAILBOX_DEPTH || mb.head >= MAILBOX_DEPTH || mb.tail >= MAILBOX_DEPTH {
                crate::serial::serial_printf(format_args!(
                    "[IPC-AUDIT] WARNING: Slot {} corrupted! len={} head={} tail={}. Resetting.\n",
                    i, mb.len, mb.head, mb.tail
                ));
                mb.clear();
            }
        }
    });
}

/// Recibir mensaje para un proceso (O(1)).
pub fn receive_message(pid: ClientId) -> Option<Message> {
    run_critical(|| {
        // 1. Intentar slot de proceso (P2P / Mailbox)
        if let Some(slot) = pid_to_slot_fast(pid) {
            #[cfg(not(test))]
            if pid == 1 {
                crate::serial::serial_printf(format_args!(
                    "[IPC-TRACE] receive_message PID1 slot={} acquiring PROCESS_MAILBOXES lock...\n",
                    slot
                ));
            }
            let mut mailboxes = PROCESS_MAILBOXES.lock();
            #[cfg(not(test))]
            if pid == 1 {
                crate::serial::serial_printf(format_args!(
                    "[IPC-TRACE] receive_message PID1 got PROCESS_MAILBOXES lock\n"
                ));
            }
            if let Some(msg) = mailboxes[slot].pop() {
                return Some(msg);
            }
        }

        // 2. Intentar como Kernel Server (Cola interna de IPC_SYSTEM)
        let mut ipc = IPC_SYSTEM.lock();
        for i in 0..32 {
            if let Some(ref mut server) = ipc.servers[i] {
                if server.id == pid {
                    let head = server.queue_head;
                    if head != server.queue_tail {
                        let msg = server.message_queue[head].take();
                        server.queue_head = (head + 1) % 64;
                        return msg;
                    }
                    return None;
                }
            }
        }
        None
    })
}

/// Recibir el primer mensaje que cumpla el predicado para un proceso (O(N) en mailbox_size).
/// NOTA: Solo busca en el Mailbox P2P, no en colas de Kernel Servers (que son FIFOs estrictos).
pub fn receive_message_filtered<F>(pid: ClientId, filter: F) -> Option<Message> 
where F: Fn(&Message) -> bool {
    run_critical(|| {
        if let Some(slot) = pid_to_slot_fast(pid) {
            return PROCESS_MAILBOXES.lock()[slot].pop_filtered(filter);
        }
        None
    })
}

/// Limpiar el buzón de un slot al terminar el proceso.
pub fn clear_mailbox_slot(slot_idx: usize) {
    if slot_idx < MAX_PROCESSES {
        run_critical(|| {
            PROCESS_MAILBOXES.lock()[slot_idx].clear();
        });
    }
}

/// Fast path: extrae del mailbox SOLO si data_size ≤ 24 bytes (cabe en registros CPU).
pub fn pop_small_message(pid: ClientId) -> Option<Message> {
    run_critical(|| {
        if let Some(slot) = pid_to_slot_fast(pid) {
            let mut mailboxes = PROCESS_MAILBOXES.lock();
            let mb = &mut mailboxes[slot];
            if let Some(front) = mb.peek() {
                if front.data_size <= 24 {
                    return mb.pop();
                }
            }
        }
        None
    })
}

/// Igual que pop_small_message pero devuelve solo (data_size, from, data[24]) para no poner
/// un Message completo (~288 bytes) en la pila del syscall y reducir riesgo de overflow/corrupción.
pub fn pop_small_message_24(pid: ClientId) -> Option<(u32, u32, [u8; 24])> {
    run_critical(|| {
        if let Some(slot) = pid_to_slot_fast(pid) {
            let mut mailboxes = PROCESS_MAILBOXES.lock();
            let mb = &mut mailboxes[slot];
            if let Some(front) = mb.peek() {
                if front.data_size <= 24 {
                    let msg = mb.pop().unwrap();
                    let mut data = [0u8; 24];
                    data[..24].copy_from_slice(&msg.data[..24]);
                    return Some((msg.data_size, msg.from, data));
                }
            }
        }
        None
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mailbox_push_pop() {
        let mut mb = ProcessMailbox::new();
        let mut msg = Message::new();
        msg.id = 123;
        msg.data[0] = 42;
        msg.data_size = 1;

        assert!(mb.push(msg));
        assert_eq!(mb.len, 1);

        let popped = mb.pop().unwrap();
        assert_eq!(popped.id, 123);
        assert_eq!(popped.data[0], 42);
        assert_eq!(mb.len, 0);
    }

    #[test]
    fn test_mailbox_overflow() {
        let mut mb = ProcessMailbox::new();
        for i in 0..MAILBOX_DEPTH {
            let mut msg = Message::new();
            msg.id = i as u64;
            assert!(mb.push(msg));
        }
        assert_eq!(mb.len, MAILBOX_DEPTH);
        assert!(!mb.push(Message::new()));
    }

    #[test]
    fn test_pid_slot_mapping() {
        let pid = 1001;
        let slot = 5;
        register_pid_slot(pid, slot);
        assert_eq!(pid_to_slot_fast(pid), Some(slot));
        unregister_pid_slot(pid);
    }

    #[test]
    fn test_send_receive_p2p() {
        let from = 10;
        let to = 20;
        let slot = 5;
        register_pid_slot(to, slot);
        
        let data = b"hello p2p";
        assert!(send_message(from, to, MessageType::Input, data));
        
        let msg = pop_small_message(to).expect("Should have a message");
        assert_eq!(msg.from, from);
        assert_eq!(msg.to, to);
        assert_eq!(msg.data_size, data.len() as u32);
        assert_eq!(&msg.data[..msg.data_size as usize], data);
        
        unregister_pid_slot(to);
    }

    #[test]
    fn test_message_routing() {
        // Test server registration and global queue routing
        let s_pid = 100;
        let s_slot = 10;
        register_pid_slot(s_pid, s_slot);
        
        let sid = register_server(b"test_svc", MessageType::System, 10)
            .expect("Failed to register server");
        
        let client_pid = 200;
        let data = b"global queue test";
        
        // Should go to global queue
        assert!(send_message(client_pid, sid, MessageType::System, data));
        
        // Process messages to move from global queue to mailbox
        process_messages();
        
        let msg = receive_message(sid).expect("Server should have received message");
        assert_eq!(msg.from, client_pid);
        assert_eq!(msg.to, sid);
        assert_eq!(&msg.data[..msg.data_size as usize], data);
        
        unregister_server(sid);
        unregister_pid_slot(s_pid);
    }

    #[test]
    fn test_ipc_throughput() {
        // Benchmark IPC delivery on host
        let s_pid = 300;
        let s_slot = 30;
        register_pid_slot(s_pid, s_slot);
        
        // P2P Delivery (Mailbox)
        let c_pid = 400;
        let c_slot = 40;
        register_pid_slot(c_pid, c_slot);
        
        let msg_count = 100_000;
        let data = b"perf test data";
        
        println!("Starting IPC throughput test ({} messages)...", msg_count);
        let start = std::time::Instant::now();
        
        for _ in 0..msg_count {
            // Send P2P (bypass routing for pure throughput)
            send_message(c_pid, s_pid, MessageType::Signal, data);
            let _ = receive_message(s_pid).unwrap();
        }
        
        let duration = start.elapsed();
        let mps = (msg_count as f64) / duration.as_secs_f64();
        
        println!("Completed in {:?}", duration);
        println!("Throughput (One-way): {:.2} messages/sec", mps);
        
        // Round-trip (Request-Response)
        println!("\nStarting Round-trip benchmark ({} iterations)...", msg_count/2);
        let start = std::time::Instant::now();
        
        for _ in 0..(msg_count/2) {
            // Client -> Server
            send_message(c_pid, s_pid, MessageType::Signal, data);
            let _ = receive_message(s_pid).unwrap();
            
            // Server -> Client (Reply)
            send_message(s_pid, c_pid, MessageType::Signal, data);
            let _ = receive_message(c_pid).unwrap();
        }
        
        let duration = start.elapsed();
        let rps = ((msg_count/2) as f64) / duration.as_secs_f64();
        
        println!("Completed in {:?}", duration);
        println!("Round-trips per second: {:.2} RPS", rps);
        println!("Approx message throughput: {:.2} messages/sec", rps * 2.0);
        
        unregister_pid_slot(s_pid);
        unregister_pid_slot(c_pid);
    }

    #[test]
    fn test_input_event_delivery() {
        // Simulate Input Service (From: 500) -> Compositor (To: 600)
        let compositor_pid = 600;
        let compositor_slot = 44;
        register_pid_slot(compositor_pid, compositor_slot);
        
        let input_svc_pid = 500;
        
        // Define an InputEvent-like data structure (matches libc)
        #[repr(C)]
        struct RawInputEvent {
            device_id: u32,
            event_type: u8,
            code: u16,
            value: i32,
            timestamp: u64,
        }
        
        // 1. Keyboard Event
        let kbd_event = RawInputEvent {
            device_id: 1,
            event_type: 0, // Key
            code: 30,      // 'A' scancode
            value: 1,      // Pressed
            timestamp: 1000,
        };
        
        let kbd_data = unsafe {
            core::slice::from_raw_parts(
                &kbd_event as *const _ as *const u8,
                core::mem::size_of::<RawInputEvent>()
            )
        };
        
        // 2. Mouse Event
        let mouse_event = RawInputEvent {
            device_id: 2,
            event_type: 1, // Mouse Move
            code: 0,
            value: 10,     // dx
            timestamp: 1001,
        };
        
        let mouse_data = unsafe {
            core::slice::from_raw_parts(
                &mouse_event as *const _ as *const u8,
                core::mem::size_of::<RawInputEvent>()
            )
        };
        
        // Send both via fast-path (MessageType::Input)
        assert!(send_message(input_svc_pid, compositor_pid, MessageType::Input, kbd_data));
        assert!(send_message(input_svc_pid, compositor_pid, MessageType::Input, mouse_data));
        
        // Verify Keyboard Event
        let msg1 = receive_message(compositor_pid).expect("Should receive kbd event");
        assert_eq!(msg1.from, input_svc_pid);
        assert_eq!(msg1.msg_type, MessageType::Input);
        
        let mut received_kbd = RawInputEvent { device_id: 0, event_type: 0, code: 0, value: 0, timestamp: 0 };
        unsafe {
            core::ptr::copy_nonoverlapping(
                msg1.data.as_ptr(),
                &mut received_kbd as *mut _ as *mut u8,
                core::mem::size_of::<RawInputEvent>()
            );
        }
        assert_eq!(received_kbd.device_id, 1);
        assert_eq!(received_kbd.code, 30);
        
        // Verify Mouse Event
        let msg2 = receive_message(compositor_pid).expect("Should receive mouse event");
        assert_eq!(msg2.from, input_svc_pid);
        
        let mut received_mouse = RawInputEvent { device_id: 0, event_type: 0, code: 0, value: 0, timestamp: 0 };
        unsafe {
            core::ptr::copy_nonoverlapping(
                msg2.data.as_ptr(),
                &mut received_mouse as *mut _ as *mut u8,
                core::mem::size_of::<RawInputEvent>()
            );
        }
        assert_eq!(received_mouse.device_id, 2);
        assert_eq!(received_mouse.value, 10);
        
        unregister_pid_slot(compositor_pid);
    }
}
