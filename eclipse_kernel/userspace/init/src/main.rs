//! Eclipse Init - Sistema de inicialización para Eclipse OS Microkernel
//! 
//! Este es el primer proceso de userspace que arranca el kernel.
//! Gestiona el montaje del sistema de archivos y los servicios del sistema.

extern crate std;
use std::prelude::v1::*;
use eclipse_libc::{getpid, sleep_ms, yield_cpu, wait, Spinlock};
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
    pid: i32,           // Process ID of the service (0 if not running)
    last_heartbeat: u64, // Uptime tick of the last heartbeat
    watchdog_enabled: bool, // Support for active heartbeat monitoring
}

impl Service {
    const fn new(name: &'static str, watchdog_enabled: bool) -> Self {
        Service {
            name,
            state: ServiceState::Stopped,
            restart_count: 0,
            pid: 0,
            last_heartbeat: 0,
            watchdog_enabled,
        }
    }
}

/// Watchdog settings
const HEARTBEAT_TIMEOUT_TICKS: u64 = 30000; // 30 seconds (assuming 1ms ticks)
const HEARTBEAT_CHECK_INTERVAL: u64 = 5000; // Check every 5 seconds

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
    Service::new("kernel", false),
    Service::new("init", false),
    Service::new("log", false),
    Service::new("devfs", false),
    Service::new("filesystem", false),
    Service::new("input", true),
    Service::new("display", false),
    Service::new("audio", false),
    Service::new("network", false),
    // gui_service is a one-shot launcher: it starts smithay_app and then exits.
    // Don't enable heartbeat watchdog for it.
    Service::new("gui", false),
]);



fn main() {
    let pid = unsafe { getpid() };
    
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
    let log_pid = start_essential_service(0); // log
    wait_for_ready(log_pid, "log", 5000);
    std::thread::sleep(std::time::Duration::from_millis(5000));
    let devfs_pid = start_essential_service(1); // devfs
    wait_for_ready(devfs_pid, "devfs", 5000);
    std::thread::sleep(std::time::Duration::from_millis(5000));
}

fn start_essential_service(service_id: u32) -> u32 {
    let name = match service_id {
        0 => "log",
        1 => "devfs",
        _ => "service",
    };
    let pid = unsafe { eclipse_libc::spawn_service(service_id, name.as_ptr(), name.len()) };
    if pid != u64::MAX {
        let mut svc = SERVICES.lock();
        let idx = (service_id + 2) as usize; // log=2, devfs=3
        svc[idx].pid = pid as i32;
        svc[idx].state = ServiceState::Running;
        println!("  [ESSENTIAL] {} started with PID: {}", svc[idx].name, pid);
        pid as u32
    } else {
        println!("  [ERROR] Failed to spawn essential service: ID {}", service_id);
        0
    }
}

/// Start system services
fn start_system_services() {
    // Start remaining services in order: filesystem, input, display, audio, network, gui
    // Indices 4 to 9 in SERVICES match IDs 2 to 7 in sys_spawn_service
    for i in 5..=9 {
        let (name, pid) = {
            let mut svc = SERVICES.lock();
            let name = svc[i].name;
            let service_id = (i - 2) as u32; // Map index to sys_spawn_service ID
            let pid = unsafe { eclipse_libc::spawn_service(service_id, name.as_ptr(), name.len()) };
            
            if pid != u64::MAX {
                svc[i].pid = pid as i32;
                svc[i].state = ServiceState::Running;
                println!("  [SERVICE] {} started with PID: {}", svc[i].name, pid);
            } else {
                println!("  [ERROR] Failed to spawn service: {}", svc[i].name);
                svc[i].state = ServiceState::Failed;
            }
            (svc[i].name, pid as u32)
        };
        if pid > 0 {
            let timeout = if name == "filesystem" { 15000 } else { 5000 };
            wait_for_ready(pid, name, timeout);
            // gui_service is a one-shot launcher, but we keep its PID to track heartbeats
            // until it exits naturally.
        }
        std::thread::sleep(std::time::Duration::from_millis(5000));
    }
}

/// Wait for a service to signal READY via IPC.
/// Uses a tight poll with yield_cpu + periodic sleep_ms to balance
/// responsiveness with CPU efficiency on SMP.
fn wait_for_ready(expected_pid: u32, name: &str, timeout_ms: u32) {
    let mut buffer = [0u8; 128];
    let mut attempts = 0u32;
    let max_attempts = timeout_ms; // 1 attempt per ms

    while attempts < max_attempts {
        // Poll mailbox a few times per ms to catch messages quickly
        for _ in 0..10 {
            let (len, sender) = eclipse_libc::receive_ipc(&mut buffer);
            if len > 0 {
                if len >= 5 && &buffer[..5] == b"READY" {
                    if sender == expected_pid {
                        println!("[INIT] Service '{}' is READY (PID {})", name, sender);
                        return;
                    } else {
                        // READY from someone else - might be a service that we aren't waiting for yet,
                        // or an app like smithay_app. Mark it as READY if it matches any service.
                        let mut svc = SERVICES.lock();
                        for s in svc.iter_mut() {
                            if s.pid == sender as i32 {
                                s.state = ServiceState::Running;
                                break;
                            }
                        }
                    }
                } else {
                    process_single_ipc_request(&buffer, len, sender);
                }
            }
            unsafe { yield_cpu(); }
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
        attempts += 1;
    }

    println!("[INIT] WARNING: Timeout waiting for service '{}' (PID {}) to signal READY", name, expected_pid);
}


/// Main loop - monitor services and handle system events
fn main_loop() -> ! {
    let mut counter: u64 = 0;
    let mut heartbeat_counter: u64 = 0;
    let mut ipc_buffer = [0u8; 128];
 
    loop {
        counter += 1;
        
        // Procesar solicitudes IPC ligeras (por ejemplo, petición de PIDs de servicios)
        handle_ipc_requests(&mut ipc_buffer);

        // Check service health every 1000 iterations (~1 s with 1 ms sleep)
        if counter % 1000 == 0 {
            check_services();
            
            // Active Watchdog: Check for heartbeat timeouts every CHECK_INTERVAL
            if counter % HEARTBEAT_CHECK_INTERVAL == 0 {
                let mut stats = eclipse_libc::SystemStats {
            uptime_ticks: 0,
            idle_ticks: 0,
            total_mem_frames: 0,
            used_mem_frames: 0,
            cpu_count: 0,
            cpu_temp: [0; 16],
            gpu_load: [0; 4],
            gpu_temp: [0; 4],
            gpu_vram_total_bytes: 0,
            gpu_vram_used_bytes: 0,
            anomaly_count: 0,
            heap_fragmentation: 0,
            wall_clock_ms: 0,
        };
                unsafe { eclipse_libc::get_system_stats(&mut stats); }
                let now = stats.wall_clock_ms;
                for i in 0..svc.len() {
                    let service = &mut svc[i];
                    if service.state == ServiceState::Running && service.pid > 0 && service.watchdog_enabled {
                        // Skip checking if heartbeat is 0 (just started and haven't gotten first HEART yet)
                        if service.last_heartbeat > 0 && now > service.last_heartbeat + HEARTBEAT_TIMEOUT_TICKS {
                            println!("[INIT] WATCHDOG: Service '{}' (PID {}) HUNG (no heartbeat for {}ms). Killing...", 
                                     service.name, service.pid, now - service.last_heartbeat);
                            unsafe { eclipse_libc::kill(service.pid as i32, 9); }
                            // Reap_zombies will handle the state transition to Failed in the next iteration
                        }
                    }
                }
            }
        }
        
        // Handle zombie processes - reap terminated children
        reap_zombies();
        
        // Sleep briefly to avoid a busy-loop; this blocks init for 1 ms
        // so the kernel can HLT and CPU usage drops from ~100% to near 0%.
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// Atender solicitudes IPC sencillas dirigidas a init (PID 1).
/// Actualmente soporta:
/// - "GET_INPUT_PID": devuelve el PID del servicio de entrada en un mensaje "INPT" + u32 LE.
fn handle_ipc_requests(buffer: &mut [u8; 128]) {
    // Drain all pending IPC messages in one pass so that with SMP multiple
    // processes queuing messages simultaneously are all handled without delay.
    loop {
        let (len, sender) = eclipse_libc::receive_ipc(buffer);
        if len == 0 || sender == 0 {
            break;
        }

        process_single_ipc_request(buffer, len, sender);
    }
}

/// Helper function to process a single IPC request.
/// This allows processing messages directly from wait_for_ready's receive loop.
fn process_single_ipc_request(buffer: &[u8], len: usize, sender: u32) {
    // Recognize and ignore "READY" messages in the main loop to avoid noise.
    if len >= 5 && &buffer[..5] == b"READY" {
        return;
    }

    // Heartbeat message ("HEART")
    if len >= 5 && &buffer[..5] == b"HEART" {
        let mut svc = SERVICES.lock();
        let mut matched = false;
        for service in svc.iter_mut() {
            if service.pid == sender as i32 {
                let mut stats = eclipse_libc::SystemStats {
            uptime_ticks: 0,
            idle_ticks: 0,
            total_mem_frames: 0,
            used_mem_frames: 0,
            cpu_count: 0,
            cpu_temp: [0; 16],
            gpu_load: [0; 4],
            gpu_temp: [0; 4],
            gpu_vram_total_bytes: 0,
            gpu_vram_used_bytes: 0,
            anomaly_count: 0,
            heap_fragmentation: 0,
            wall_clock_ms: 0,
        };
                unsafe { eclipse_libc::get_system_stats(&mut stats); }
                service.last_heartbeat = stats.wall_clock_ms;
                matched = true;
                break;
            }
        }
        if !matched {
            println!("[INIT] Heartbeat from unknown PID: {} (matches none in service list)", sender);
        }
        return;
    }

    // Petición de PID del servicio de entrada ("GET_INPUT_PID" = 13 bytes)
    // Tolerancia a nulos y variaciones de longitud
    if (len >= 13 && &buffer[..13] == b"GET_INPUT_PID") || (len >= 14 && &buffer[..14] == b"GET_INPUT_PID\0") {
        let input_pid = SERVICES.lock()[5].pid as u32; // Servicio "input"
        println!("[INIT] IPC: Sending input PID {} to requesting process {}", input_pid, sender);
        let mut response = [0u8; 8];
        response[0..4].copy_from_slice(b"INPT");
        response[4..8].copy_from_slice(&input_pid.to_le_bytes());
        // Use MSG_TYPE_INPUT (0x40 = P2P) so the response is delivered directly to
        // the requester's mailbox instead of being dropped in the global IPC queue.
        let _ = eclipse_libc::send_ipc(sender, 0x40, &response);
        return;
    }

    // Petición de PID del servicio de pantalla ("GET_DISPLAY_PID" = 15 bytes)
    if (len >= 15 && &buffer[..15] == b"GET_DISPLAY_PID") || (len >= 16 && &buffer[..16] == b"GET_DISPLAY_PID\0") {
        let display_pid = SERVICES.lock()[6].pid as u32; // Servicio "display" (Smithay)
        println!("[INIT] IPC: Sending display PID {} to requesting process {}", display_pid, sender);
        let mut response = [0u8; 8];
        response[0..4].copy_from_slice(b"DSPL");
        response[4..8].copy_from_slice(&display_pid.to_le_bytes());
        let _ = eclipse_libc::send_ipc(sender, 0x40, &response);
        return;
    }

    // Petición de PID del servicio de red ("GET_NETWORK_PID" = 15 bytes)
    if (len >= 15 && &buffer[..15] == b"GET_NETWORK_PID") || (len >= 16 && &buffer[..16] == b"GET_NETWORK_PID\0") {
        let net_pid = SERVICES.lock()[8].pid as u32; // Servicio "network"
        println!("[INIT] IPC: Sending network PID {} to requesting process {}", net_pid, sender);
        let mut response = [0u8; 8];
        response[0..4].copy_from_slice(b"NETW");
        response[4..8].copy_from_slice(&net_pid.to_le_bytes());
        let _ = eclipse_libc::send_ipc(sender, 0x40, &response);
        return;
    }

    // Petición de información de servicios ("GET_SERVICES_INFO")
    if (len >= 17 && &buffer[..17] == b"GET_SERVICES_INFO") || (len >= 18 && &buffer[..18] == b"GET_SERVICES_INFO\0") {
        let mut reply = [0u8; 512]; // Aumentado para soportar más servicios
        reply[0..4].copy_from_slice(b"SVCS");
        let mut offset = 8;
        let svc_count;
        {
            let svc = SERVICES.lock();
            svc_count = svc.len();
            reply[4..8].copy_from_slice(&(svc_count as u32).to_le_bytes());
            // Format: [name: 16 bytes][state: u32][pid: u32][restart_count: u32] = 28 bytes per service
            // 10 services * 28 bytes = 280 bytes, fits within the 512-byte buffer.
            for s in svc.iter() {
                if offset + 28 > 512 { break; }
                let name_bytes = s.name.as_bytes();
                let name_len = name_bytes.len().min(16);
                reply[offset..offset + name_len].copy_from_slice(&name_bytes[..name_len]);
                offset += 16;
                reply[offset..offset + 4].copy_from_slice(&(s.state as u32).to_le_bytes());
                offset += 4;
                reply[offset..offset + 4].copy_from_slice(&(s.pid as u32).to_le_bytes());
                offset += 4;
                reply[offset..offset + 4].copy_from_slice(&s.restart_count.to_le_bytes());
                offset += 4;
            }
        }
        let _ = eclipse_libc::send_ipc(sender, 0x40, &reply[..offset]);
        return;
    }

    // Heartbeat ("HEART")
    if len >= 5 && &buffer[..5] == b"HEART" {
        // No loggeamos para no inundar, pero confirmamos recepción
        return;
    }


    // Otros mensajes se registran para depuración básica
    println!(
        "[INIT] IPC no reconocido ({} bytes desde PID {})",
        len, sender
    );
    print_hex_dump(buffer, len.min(32));
}

fn print_hex_dump(data: &[u8], len: usize) {
    let mut line = [0u8; 64];
    let mut pos = 0;
    for i in 0..len {
        let b = data[i];
        let h1 = b >> 4;
        let h2 = b & 0x0F;
        line[pos] = if h1 < 10 { b'0' + h1 } else { b'A' + h1 - 10 };
        line[pos+1] = if h2 < 10 { b'0' + h2 } else { b'A' + h2 - 10 };
        line[pos+2] = b' ';
        pos += 3;
        if pos >= 60 { break; }
    }
    if pos > 0 {
        if let Ok(s) = core::str::from_utf8(&line[..pos]) {
            println!("  [DUMP] {}", s);
        }
    }
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
        let mut stats = eclipse_libc::SystemStats {
            uptime_ticks: 0,
            idle_ticks: 0,
            total_mem_frames: 0,
            used_mem_frames: 0,
            cpu_count: 0,
            cpu_temp: [0; 16],
            gpu_load: [0; 4],
            gpu_temp: [0; 4],
            gpu_vram_total_bytes: 0,
            gpu_vram_used_bytes: 0,
            anomaly_count: 0,
            heap_fragmentation: 0,
            wall_clock_ms: 0,
        };
        unsafe { eclipse_libc::get_system_stats(&mut stats); }
        let now = stats.wall_clock_ms;

        for (i, service) in svc.iter().enumerate() {
            // Stable uptime reset: if a service has been running for > 60s, reset its restart count
            if service.state == ServiceState::Running && service.last_heartbeat > 0 && 
               now > service.last_heartbeat + 60000 {
                // We need to mutate, but we only have a read lock if we use iter()
                // So we'll handle this in the restart loop or just give it a mutable lock here.
            }

            if service.state == ServiceState::Failed && service.restart_count < 10 {
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
            let service_id = (idx - 2) as u32;
            let name = svc[idx].name;
            let pid = unsafe { eclipse_libc::spawn_service(service_id, name.as_ptr(), name.len()) };
            
            if pid != u64::MAX {
                svc[idx].pid = pid as i32;
                svc[idx].state = ServiceState::Running;
                svc[idx].restart_count += 1;
                println!("[INIT] Service {} restarted with PID: {}", name, pid);
            } else {
                println!("[INIT] ERROR: Failed to restart service: {}", name);
                svc[idx].state = ServiceState::Failed;
            }
        }
    }
}

/// Reap zombie processes and update service states
fn reap_zombies() {
    loop {
        // Non-blocking wait for any terminated child
        let terminated_pid = unsafe { wait(core::ptr::null_mut()) };
        
        if terminated_pid < 0 {
            // No more terminated children
            break;
        }
        
        // Find which service this PID belonged to
        let mut svc = SERVICES.lock();
        for service in svc.iter_mut() {
            if service.pid == terminated_pid && service.state == ServiceState::Running {
                if service.name == "gui" {
                    // One-shot: gui_service exiting is expected once it has launched smithay_app.
                    println!("[INIT] Service {} (PID {}) has completed (one-shot)", service.name, terminated_pid);
                    service.state = ServiceState::Stopped;
                } else {
                    println!("[INIT] Service {} (PID {}) has terminated", 
                             service.name, terminated_pid);
                    service.state = ServiceState::Failed;
                }
                service.pid = 0;
                break;
            }
        }
    }
}