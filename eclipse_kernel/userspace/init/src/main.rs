//! Eclipse Init - Sistema de inicialización para Eclipse OS Microkernel
//! 
//! Este es el primer proceso de userspace que arranca el kernel.
//! Gestiona el montaje del sistema de archivos y los servicios del sistema.

#![no_std]
#![no_main]

use eclipse_libc::{println, getpid, yield_cpu, fork, exec, wait, exit, get_service_binary, get_last_exec_error};

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

/// System services
/// Launch order (as per requirements):
/// 1. Log Server / Console (0)
/// 2. Device Manager (devfs) (1)
/// 3. Filesystem Server (2)
/// 4. Input Server (3)
/// 5. Graphics Server (Display) (4)
/// 6. Audio Server (5)
/// 7. Network Server (6)
static mut SERVICES: [Service; 8] = [
    Service::new("log"),
    Service::new("devfs"),
    Service::new("filesystem"),
    Service::new("input"),
    Service::new("display"),
    Service::new("audio"),
    Service::new("network"),
    Service::new("gui"),
];



#[no_mangle]
pub extern "C" fn _start() -> ! {
    let pid = getpid();
    
    // Usamos solo ASCII aquí para evitar posibles problemas con el formateo
    // de caracteres Unicode complejos en las primeras trazas de arranque.
    println!("==============================================================");
    println!("==          ECLIPSE OS INIT SYSTEM v0.1.3-FIXED             ==");
    println!("==============================================================");
    println!();
    println!("Init process started with PID: {}", pid);
    println!();
    
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
    unsafe {
        // Start log server first - critical for debugging
        start_service(&mut SERVICES[0]);
        
        // Wait for log service to be ready
        println!("[INIT] Waiting for LOG service to signal READY...");
        wait_for_ready("log", 5000);
        
        // Start device manager (devfs) - creates /dev nodes
        start_service(&mut SERVICES[1]);
        
        // Wait for devfs to be ready
        println!("[INIT] Waiting for DevFS to signal READY...");
        wait_for_ready("devfs", 5000);
    }
}

/// Start system services
fn start_system_services() {
    unsafe {
        // Start filesystem service (depends on devfs)
        start_service(&mut SERVICES[2]);
        
        // Wait for filesystem service to mount the disk
        println!("[INIT] Waiting for Filesystem service to signal READY...");
        wait_for_ready("filesystem", 10000); // Give it more time
        println!("  [FS] Root filesystem ready (mounted by filesystem service).");

        // Start input service (depends on filesystem)
        start_service(&mut SERVICES[3]);
        wait_for_ready("input", 5000);
        
        // Start display service (depends on input)
        start_service(&mut SERVICES[4]);
        wait_for_ready("display", 5000);

        // Start audio service (depends on filesystem)
        start_service(&mut SERVICES[5]);
        wait_for_ready("audio", 5000);
        
        // Start network service last (most complex)
        start_service(&mut SERVICES[6]);
        wait_for_ready("network", 5000);

        // Start GUI service (depends on network)
        start_service(&mut SERVICES[7]);
        wait_for_ready("gui", 5000);
    }
}

/// Wait for a service to signal READY via IPC
fn wait_for_ready(name: &str, timeout_ms: u32) {
    let mut buffer = [0u8; 32];
    let mut attempts = 0;
    let max_attempts = timeout_ms / 10; // Yield every 10ms approx
    
    while attempts < max_attempts {
        let (len, _sender) = eclipse_libc::receive(&mut buffer);
        if len > 0 {
            if len >= 5 && &buffer[..5] == b"READY" {
                println!("[INIT] Service '{}' is READY", name);
                return;
            } else {
                // If we got another message, just log it for now
                println!("[INIT] Received unexpected IPC during wait for '{}': {} bytes", name, len);
            }
        }
        
        yield_cpu();
        attempts += 1;
        
        if attempts % 100 == 0 {
            println!("[INIT] Still waiting for '{}' ({}%)...", name, (attempts * 100) / max_attempts);
        }
    }
    
    println!("[INIT] WARNING: Timeout waiting for service '{}' to signal READY", name);
}

/// Start a service
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

        // Check service health every 100000 iterations
        if counter % 100000 == 0 {
            check_services();
        }
        
        // Print heartbeat every 1000000 iterations
        if counter % 1000000 == 0 {
            heartbeat_counter += 1;
            println!("[INIT] Heartbeat #{} - System operational", heartbeat_counter);
            print_service_status();
        }
        
        // Handle zombie processes - reap terminated children
        reap_zombies();
        
        // Yield CPU to other processes
        yield_cpu();
    }
}

/// Atender solicitudes IPC sencillas dirigidas a init (PID 1).
/// Actualmente soporta:
/// - "GET_INPUT_PID": devuelve el PID del servicio de entrada en un mensaje "INPT" + u32 LE.
fn handle_ipc_requests(buffer: &mut [u8; 32]) {
    let (len, sender) = eclipse_libc::receive(buffer);
    if len == 0 || sender == 0 {
        return;
    }

    // Petición de PID del servicio de entrada ("GET_INPUT_PID" = 13 bytes)
    if len >= 13 && &buffer[..13] == b"GET_INPUT_PID" {
        let input_pid = unsafe { SERVICES[3].pid as u32 }; // Servicio "input"
        let mut response = [0u8; 8];
        response[0..4].copy_from_slice(b"INPT");
        response[4..8].copy_from_slice(&input_pid.to_le_bytes());
        let _ = eclipse_libc::send(sender, 0x40, &response);
        return;
    }

    // Otros mensajes se registran para depuración básica
    println!(
        "[INIT] IPC no reconocido en main_loop: {} bytes desde PID {}",
        len, sender
    );
}

/// Check service health
fn check_services() {
    unsafe {
        for service in SERVICES.iter_mut() {
            if service.state == ServiceState::Running {
                // Process is tracked via PID, wait() will detect if it terminates
            } else if service.state == ServiceState::Failed {
                // Implement restart policy
                if service.restart_count < 3 {
                    println!("[INIT] Restarting failed service: {} (attempt {})", 
                             service.name, service.restart_count + 1);
                    start_service(service);
                    service.restart_count += 1;
                }
            }
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
        unsafe {
            for service in SERVICES.iter_mut() {
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
}

/// Print service status
fn print_service_status() {
    unsafe {
        println!("[INIT] Service Status:");
        for service in SERVICES.iter() {
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
}

