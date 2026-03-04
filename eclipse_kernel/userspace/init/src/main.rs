//! Eclipse Init - Sistema de inicialización para Eclipse OS Microkernel
//! 
//! Este es el primer proceso de userspace que arranca el kernel.
//! Gestiona el montaje del sistema de archivos y los servicios del sistema.

#![no_std]
#![no_main]

use eclipse_libc::{println, getpid, sleep_ms, yield_cpu, wait, spawn_service, Spinlock};
use eclipse_libc::{fork, exec, get_service_binary, get_last_exec_error, exit};

/// Service state
#[derive(Clone, Copy, PartialEq)]
enum ServiceState {
    Stopped,
    Starting,
    Running,
    Failed,
}

/// Service definition
struct Service {
    name: &'static str,
    state: ServiceState,
    restart_count: u32,
    pid: i32,  // Process ID of the service (0 if not running)
}

impl Service {
    const fn new(name: &'static str) -> Self {
        Service {
            name,
            state: ServiceState::Stopped,
            restart_count: 0,
            pid: 0,
        }
    }
}

/// System services protected by a spinlock for thread-safe SMP access.
/// Launch order (as per requirements):
/// 1. Log Server / Console (0)
/// 2. Device Manager (devfs) (1)
/// 3. Filesystem Server (2)
/// 4. Input Server (3)
/// 5. Graphics Server (Display) (4)
/// 6. Audio Server (5)
/// 7. Network Server (6)
static SERVICES: Spinlock<[Service; 10]> = Spinlock::new([
    Service::new("kernel"),
    Service::new("init"),
    Service::new("log"),
    Service::new("devfs"),
    Service::new("filesystem"),
    Service::new("input"),
    Service::new("display"),
    Service::new("audio"),
    Service::new("network"),
    Service::new("gui"),
]);



#[no_mangle]
pub extern "C" fn _start() -> ! {
    let pid = getpid();
    
    // Usamos solo ASCII aquí para evitar posibles problemas con el formateo
    // de caracteres Unicode complejos en las primeras trazas de arranque.
    println!("==============================================================");
    println!("==          ECLIPSE OS INIT SYSTEM v0.1.3-DEBUG-REALLY     ==");
    println!("==============================================================");
    println!();
    println!("Init process started with PID: {}", pid);
    println!();

    {
        let mut svc = SERVICES.lock();
        // Initialize kernel and init services which are already running
        svc[0].state = ServiceState::Running;
        svc[0].pid = 0;
        svc[1].state = ServiceState::Running;
        svc[1].pid = pid as i32;
    }
    
    // Phase 1: Start essential services (log, devfs). Root is NOT mounted yet.
    println!("[INIT] Phase 1: Starting essential services (log, devfs)...");
    start_essential_services();
    println!();
    
    // Phase 2: Start filesystem service; it mounts the root. Then start rest of system services.
    println!("[INIT] Phase 2: Starting system services (FS, input, display, etc.)...");
    start_system_services();
    println!();
    
    // Phase 3: Enter main loop
    println!("[INIT] Phase 3: Entering main loop...");
    println!("[INFO] Init process running. System operational.");
    println!();
    
    main_loop();
}

/// Start essential services
fn start_essential_services() {
    // Start log server first - critical for debugging
    {
        let mut svc = SERVICES.lock();
        start_service(&mut svc[2]);
    }
    
    // Wait for log service to be ready
    println!("[INIT] Waiting for LOG service to signal READY...");
    wait_for_ready("log", 1500);
    
    // Start device manager (devfs) - creates /dev nodes
    {
        let mut svc = SERVICES.lock();
        start_service(&mut svc[3]);
    }
    
    // Wait for devfs to be ready
    println!("[INIT] Waiting for DevFS to signal READY...");
    wait_for_ready("devfs", 1500);
}

/// Start system services
fn start_system_services() {
    // Start remaining services in order: input, display, audio, network, gui
    for i in 5..=9 {
        let name = {
            let mut svc = SERVICES.lock();
            start_service(&mut svc[i]);
            svc[i].name
        };
        wait_for_ready(name, 1500);
    }
}

///

/// Wait for a service to signal READY via IPC.
/// Uses a tight poll with yield_cpu + periodic sleep_ms to balance
/// responsiveness with CPU efficiency on SMP.
fn wait_for_ready(name: &str, timeout_ms: u32) {
    let mut buffer = [0u8; 32];
    let mut attempts = 0u32;
    let max_attempts = timeout_ms; // 1 attempt per ms

    while attempts < max_attempts {
        // Poll mailbox several times per ms to catch messages quickly
        for _ in 0..50 {
            let (len, sender) = eclipse_libc::receive(&mut buffer);
            if len > 0 {
                if len >= 5 && &buffer[..5] == b"READY" {
                    println!("[INIT] Service '{}' is READY (received from PID {})", name, sender);
                    return;
                } else {
                    process_single_ipc_request(&buffer, len, sender);
                }
            }
            yield_cpu();
        }

        sleep_ms(1);
        attempts += 1;

        if attempts % 1000 == 0 {
            println!("[INIT] Still waiting for '{}' ({}%)...", name, (attempts as u64 * 100) / max_attempts as u64);
        }
    }

    println!("[INIT] WARNING: Timeout waiting for service '{}' to signal READY", name);
}

/// Start a service (must be called with the SERVICES lock held)
fn start_service(service: &mut Service) {
    println!("  [SERVICE] Starting {}...", service.name);
    
    service.state = ServiceState::Starting;
    
    // Fork a new process for the service
    let pid = fork();
    
    if pid == 0 {
        // Child process - execute the service
        
        // Determine which service binary to load
        let service_id = match service.name {
            "log" => 0,
            "devfs" => 1,
            "filesystem" => 2,
            "input" => 3,
            "display" => 4,
            "audio" => 5,
            "network" => 6,
            "gui" => 7,
            _ => {
                println!("  [CHILD] ERROR: Unknown service: {}", service.name);
                exit(1);
            }
        };
        
        // Get service binary from kernel
        let (bin_ptr, bin_size) = get_service_binary(service_id);
        
        if bin_ptr.is_null() || bin_size == 0 {
            println!("  [CHILD] ERROR: Failed to get service binary for: {} (ID {})", service.name, service_id);
            exit(1);
        }
        
        // Create slice from pointer
        let service_binary = unsafe {
            core::slice::from_raw_parts(bin_ptr, bin_size)
        };
        
        // Execute the service binary
        let _result = exec(service_binary);
        
        // If exec succeeds, it should not return
        println!("  [CHILD] ERROR: exec() failed for service: {}", service.name);
        
        // Try to get failure reason from kernel
        let mut errbuf = [0u8; 80];
        let n = get_last_exec_error(&mut errbuf);
        if n > 0 {
            if let Ok(s) = core::str::from_utf8(&errbuf[..n]) {
                println!("  [CHILD] Failure reason: {}", s);
            }
        }
        
        exit(1);
    } else if pid > 0 {
        // Parent process - track the service
        service.pid = pid;
        service.state = ServiceState::Running;
        println!("  [SERVICE] {} started with PID: {}", service.name, pid);
    } else {
        // Fork failed
        println!("  [ERROR] Failed to fork service: {}", service.name);
        service.state = ServiceState::Failed;
        service.pid = 0;
    }
}

/// Main loop - monitor services and handle system events
fn main_loop() -> ! {
    let mut counter: u64 = 0;
    let mut heartbeat_counter: u64 = 0;
    let mut ipc_buffer = [0u8; 32];
 
    loop {
        counter += 1;
        
        // Procesar solicitudes IPC ligeras (por ejemplo, petición de PIDs de servicios)
        handle_ipc_requests(&mut ipc_buffer);

        // Check service health every 1000 iterations (~1 s with 1 ms sleep)
        if counter % 1000 == 0 {
            check_services();
        }
        
        // Print heartbeat every 10000 iterations (~10 s with 1 ms sleep)
        if counter % 10000 == 0 {
            heartbeat_counter += 1;
            println!("[INIT] Heartbeat #{} - System operational", heartbeat_counter);
            print_service_status();
        }
        
        // Handle zombie processes - reap terminated children
        reap_zombies();
        
        // Sleep briefly to avoid a busy-loop; this blocks init for 1 ms
        // so the kernel can HLT and CPU usage drops from ~100% to near 0%.
        sleep_ms(1);
    }
}

/// Atender solicitudes IPC sencillas dirigidas a init (PID 1).
/// Actualmente soporta:
/// - "GET_INPUT_PID": devuelve el PID del servicio de entrada en un mensaje "INPT" + u32 LE.
fn handle_ipc_requests(buffer: &mut [u8; 32]) {
    // Drain all pending IPC messages in one pass so that with SMP multiple
    // processes queuing messages simultaneously are all handled without delay.
    loop {
        let (len, sender) = eclipse_libc::receive(buffer);
        if len == 0 || sender == 0 {
            break;
        }

        process_single_ipc_request(buffer, len, sender);
    }
}

/// Helper function to process a single IPC request.
/// This allows processing messages directly from wait_for_ready's receive loop.
fn process_single_ipc_request(buffer: &[u8], len: usize, sender: u32) {

    // Petición de PID del servicio de entrada ("GET_INPUT_PID" = 13 bytes)
    if len >= 13 && &buffer[..13] == b"GET_INPUT_PID" {
        let input_pid = SERVICES.lock()[5].pid as u32; // Servicio "input"
        let mut response = [0u8; 8];
        response[0..4].copy_from_slice(b"INPT");
        response[4..8].copy_from_slice(&input_pid.to_le_bytes());
        // Use MSG_TYPE_INPUT (0x40 = P2P) so the response is delivered directly to
        // the requester's mailbox instead of being dropped in the global IPC queue.
        let _ = eclipse_libc::send(sender, 0x40, &response);
        return;
    }

    // Petición de PID del servicio de pantalla ("GET_DISPLAY_PID" = 15 bytes)
    if len >= 15 && &buffer[..15] == b"GET_DISPLAY_PID" {
        let display_pid = SERVICES.lock()[6].pid as u32; // Servicio "display" (Smithay)
        let mut response = [0u8; 8];
        response[0..4].copy_from_slice(b"DSPL");
        response[4..8].copy_from_slice(&display_pid.to_le_bytes());
        let _ = eclipse_libc::send(sender, 0x10, &response);
        return;
    }

    // Petición de PID del servicio de red ("GET_NETWORK_PID" = 15 bytes)
    if len >= 15 && &buffer[..15] == b"GET_NETWORK_PID" {
        let net_pid = SERVICES.lock()[8].pid as u32; // Servicio "network"
        let mut response = [0u8; 8];
        response[0..4].copy_from_slice(b"NETW");
        response[4..8].copy_from_slice(&net_pid.to_le_bytes());
        // Use MSG_TYPE_INPUT (0x40 = P2P) so the response is delivered directly to
        // the requester's mailbox instead of being dropped in the global IPC queue.
        let _ = eclipse_libc::send(sender, 0x40, &response);
        return;
    }

    // Petición de información de servicios ("GET_SERVICES_INFO")
    if len >= 17 && &buffer[..17] == b"GET_SERVICES_INFO" {
        let mut reply = [0u8; 512]; // Aumentado para soportar más servicios
        reply[0..4].copy_from_slice(b"SVCS");
        let mut offset = 8;
        let svc_count;
        {
            let svc = SERVICES.lock();
            svc_count = svc.len();
            reply[4..8].copy_from_slice(&(svc_count as u32).to_le_bytes());
            // Format: [name: 12 bytes][state: u32][pid: u32][restart_count: u32] = 24 bytes per service
            // Reduced from 16-byte name to 12 to fit within the 256-byte IPC message limit
            for s in svc.iter() {
                if offset + 24 > 256 { break; }
                let name_bytes = s.name.as_bytes();
                let name_len = name_bytes.len().min(12);
                reply[offset..offset + name_len].copy_from_slice(&name_bytes[..name_len]);
                offset += 12;
                reply[offset..offset + 4].copy_from_slice(&(s.state as u32).to_le_bytes());
                offset += 4;
                reply[offset..offset + 4].copy_from_slice(&(s.pid as u32).to_le_bytes());
                offset += 4;
                reply[offset..offset + 4].copy_from_slice(&s.restart_count.to_le_bytes());
                offset += 4;
            }
        }
        let _ = eclipse_libc::send(sender, 0x40, &reply[..offset]);
        return;
    }


    // Otros mensajes se registran para depuración básica
    println!(
        "[INIT] IPC no reconocido ({} bytes desde PID {})",
        len, sender
    );
}

/// Check service health
fn check_services() {
    // Collect the indices of services that need restarting without holding the lock
    // across the spawn_service syscall (which may take time and would prevent other
    // CPUs from reading SERVICES during that window).
    let mut restart_indices: [usize; 10] = [usize::MAX; 10];
    let mut n_restart = 0;
    {
        let svc = SERVICES.lock();
        for (i, service) in svc.iter().enumerate() {
            if service.state == ServiceState::Failed && service.restart_count < 3 {
                restart_indices[n_restart] = i;
                n_restart += 1;
            }
        }
    }

    // Now restart each failed service, acquiring the lock only briefly for the update.
    for &idx in &restart_indices[..n_restart] {
        let name = SERVICES.lock()[idx].name;
        println!("[INIT] Restarting failed service: {} (attempt {})",
                 name,
                 SERVICES.lock()[idx].restart_count + 1);
        // Acquire the lock, borrow the service entry, and call start_service.
        // start_service internally calls spawn_service (a kernel syscall that does
        // not re-enter init's IPC path), so it is safe to hold the lock here.
        {
            let mut svc = SERVICES.lock();
            start_service(&mut svc[idx]);
            svc[idx].restart_count += 1;
        }
    }
}

/// Reap zombie processes and update service states
fn reap_zombies() {
    loop {
        // Non-blocking wait for any terminated child
        let terminated_pid = wait(None);
        
        if terminated_pid < 0 {
            // No more terminated children
            break;
        }
        
        // Find which service this PID belonged to
        let mut svc = SERVICES.lock();
        for service in svc.iter_mut() {
            if service.pid == terminated_pid && service.state == ServiceState::Running {
                println!("[INIT] Service {} (PID {}) has terminated", 
                         service.name, terminated_pid);
                service.state = ServiceState::Failed;
                service.pid = 0;
                break;
            }
        }
    }
}

/// Print service status
fn print_service_status() {
    let svc = SERVICES.lock();
    println!("[INIT] Service Status:");
    for service in svc.iter() {
        let status = match service.state {
            ServiceState::Stopped => "stopped",
            ServiceState::Starting => "starting",
            ServiceState::Running => "running",
            ServiceState::Failed => "failed",
        };
        if service.pid > 0 {
            println!("  - {}: {} (PID: {}, restarts: {})", 
                     service.name, status, service.pid, service.restart_count);
        } else {
            println!("  - {}: {} (restarts: {})", 
                     service.name, status, service.restart_count);
        }
    }
}

