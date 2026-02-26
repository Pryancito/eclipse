//! Sistema IPC (Inter-Process Communication) del microkernel
//! 
//! Implementa comunicación por mensajes entre procesos y servidores

use spin::Mutex;
use core::sync::atomic::{AtomicU32, Ordering};

/// ID de mensaje
pub type MessageId = u64;

/// ID de servidor
pub type ServerId = u32;

/// ID de cliente
pub type ClientId = u32;

/// Tamaño máximo de datos en un mensaje
const MAX_MESSAGE_DATA: usize = 256;

/// Tipos de mensaje
#[repr(u32)]
#[derive(Clone, Copy, PartialEq)]
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

/// Sistema IPC global
struct IpcSystem {
    servers: [Option<Server>; 32],
    clients: [Option<Client>; 256],
    message_id_counter: AtomicU32,
    server_id_counter: AtomicU32,
    client_id_counter: AtomicU32,
    global_message_queue: [Option<Message>; 1024],
    global_queue_head: usize,
    global_queue_tail: usize,
    total_messages: u64,
}

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
            global_queue_head: 0,
            global_queue_tail: 0,
            total_messages: 0,
        }
    }
}

use alloc::collections::VecDeque;

static IPC_SYSTEM: Mutex<IpcSystem> = Mutex::new(IpcSystem::new());

/// Buzones de mensajes por proceso (Process Mailboxes)
/// Mapea PID -> Cola de mensajes (FIFO)
/// Limitado a MAX_PROCESSES (64) definidos en process.rs
static PROCESS_MAILBOXES: Mutex<[Option<VecDeque<Message>>; 64]> = Mutex::new([const { None }; 64]);

/// Inicializar el sistema IPC
pub fn init() {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut ipc = IPC_SYSTEM.lock();
        // Reset del sistema
        ipc.message_id_counter.store(1, Ordering::SeqCst);
        ipc.server_id_counter.store(1, Ordering::SeqCst);
        ipc.client_id_counter.store(1, Ordering::SeqCst);
        ipc.total_messages = 0;
        
        // Reset mailboxes
        let mut mailboxes = PROCESS_MAILBOXES.lock();
        for slot in mailboxes.iter_mut() {
            *slot = None;
        }
    });
}

/// Registrar un servidor
pub fn register_server(name: &[u8], msg_type: MessageType, priority: u8) -> Option<ServerId> {
    x86_64::instructions::interrupts::without_interrupts(|| {
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

/// Registrar un cliente
pub fn register_client(name: &[u8], server_id: ServerId, permissions: u32) -> Option<ClientId> {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut ipc = IPC_SYSTEM.lock();
        let client_id = ipc.client_id_counter.fetch_add(1, Ordering::SeqCst);
        
        // Buscar slot libre
        for i in 0..256 {
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

/// Enviar un mensaje
pub fn send_message(from: ClientId, to: ServerId, msg_type: MessageType, data: &[u8]) -> bool {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut ipc = IPC_SYSTEM.lock();
        
        // Crear mensaje
        let mut msg = Message::new();
        msg.id = ipc.message_id_counter.fetch_add(1, Ordering::SeqCst) as u64;
        msg.from = from;
        msg.to = to;
        msg.msg_type = msg_type;
        
        let data_len = core::cmp::min(data.len(), MAX_MESSAGE_DATA);
        msg.data[..data_len].copy_from_slice(&data[..data_len]);
        msg.data_size = data_len as u32;
        
        // Agregar a la cola global
        let tail = ipc.global_queue_tail;
        let next_tail = (tail + 1) % 1024;
        if next_tail == ipc.global_queue_head {
            return false; // Cola llena
        }
        
        ipc.global_message_queue[tail] = Some(msg);
        ipc.global_queue_tail = next_tail;
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
    for _ in 0..32 {
        // Phase 1: Extract one message from the global queue (IPC_SYSTEM only).
        // Returns: Some(Some((pid, msg))) = P2P for mailbox delivery
        //          Some(None)             = message delivered to server (or dropped)
        //          None                   = queue empty, stop
        let p2p = x86_64::instructions::interrupts::without_interrupts(|| {
            let mut ipc = IPC_SYSTEM.lock();

            if ipc.global_queue_head == ipc.global_queue_tail {
                return None; // Queue empty - stop the outer loop
            }

            let head = ipc.global_queue_head;
            if let Some(msg) = ipc.global_message_queue[head] {
                // Signal and Input (P2P): route to Process Mailbox
                let is_p2p = msg.msg_type == MessageType::Signal
                    || msg.msg_type == MessageType::Input;
                if is_p2p {
                    let pid = msg.to as usize;
                    ipc.global_message_queue[head] = None;
                    ipc.global_queue_head = (ipc.global_queue_head + 1) % 1024;
                    if pid > 0 && pid < 64 {
                        return Some(Some((pid, msg))); // deliver to mailbox in Phase 2
                    }
                    return Some(None); // invalid pid – dropped, continue
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
                            }
                        }
                    }
                } else {
                    // No server found – clear slot so it isn't processed again
                    ipc.global_message_queue[head] = None;
                }
                ipc.global_queue_head = (ipc.global_queue_head + 1) % 1024;
            } else {
                ipc.global_queue_head = (ipc.global_queue_head + 1) % 1024;
            }
            Some(None) // continue outer loop
        });

        match p2p {
            None => break, // queue was empty
            Some(None) => {} // already handled (server delivery or dropped)
            Some(Some((pid, msg))) => {
                // Phase 2: Deliver P2P message to mailbox (PROCESS_MAILBOXES only,
                // IPC_SYSTEM is NOT held here).
                x86_64::instructions::interrupts::without_interrupts(|| {
                    let mut mailboxes = PROCESS_MAILBOXES.lock();
                    if mailboxes[pid].is_none() {
                        mailboxes[pid] = Some(VecDeque::new());
                    }
                    if let Some(queue) = &mut mailboxes[pid] {
                        if queue.len() < 256 {
                            queue.push_back(msg);
                        }
                    }
                });
            }
        }
    }
}

/// ¿Hay mensajes pendientes por procesar?
pub fn has_pending_messages() -> bool {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let ipc = IPC_SYSTEM.lock();
        ipc.global_queue_head != ipc.global_queue_tail
    })
}

/// Obtener estadísticas del sistema IPC
pub fn get_stats() -> (u32, u32, u64) {
    x86_64::instructions::interrupts::without_interrupts(|| {
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

/// Recibir mensaje para un cliente
pub fn receive_message(client_id: ClientId) -> Option<Message> {
    x86_64::instructions::interrupts::without_interrupts(|| {
        // Check Process Mailbox: P2P messages (Signal/Input) are routed here by process_messages().
        // The timer calls process_messages() every 1 ms, so messages arrive in the mailbox quickly.
        // The previous O(n) global-queue linear scan was holding IPC_SYSTEM with IRQs disabled for
        // up to 1024 iterations, starving the keyboard/mouse IRQ handlers whenever the queue grew.
        let client_pid = client_id as usize;
        if client_pid < 64 {
            let mut mailboxes = PROCESS_MAILBOXES.lock();
            if let Some(queue) = &mut mailboxes[client_pid] {
                if let Some(msg) = queue.pop_front() {
                    return Some(msg);
                }
            }
        }

        None
    })
}
