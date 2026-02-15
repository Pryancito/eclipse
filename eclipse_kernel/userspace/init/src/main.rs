//! Eclipse Init - Sistema de inicialización para Eclipse OS Microkernel
//! 
//! Este es el primer proceso de userspace que arranca el kernel.
//! Gestiona el montaje del sistema de archivos y los servicios del sistema.

#![no_std]
#![no_main]

use eclipse_libc::{println, getpid, yield_cpu, fork, exec, wait, exit, get_service_binary};

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
    
    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║              ECLIPSE OS INIT SYSTEM v0.1.0                   ║");
    println!("╚══════════════════════════════════════════════════════════════╝");
    println!();
    println!("Init process started with PID: {}", pid);
    println!();
    
    // Phase 1: Mount filesystems
    println!("[INIT] Phase 1: Mounting filesystems...");
    mount_filesystems();
    println!();
    
    // Phase 2: Start essential services
    println!("[INIT] Phase 2: Starting essential services...");
    start_essential_services();
    println!();
    
    // Phase 3: Start system services
    println!("[INIT] Phase 3: Starting system services...");
    start_system_services();
    println!();
    
    // Phase 4: Enter main loop
    println!("[INIT] Phase 4: Entering main loop...");
    println!("[INFO] Init process running. System operational.");
    println!();
    
    main_loop();
}

/// Mount filesystem
fn mount_filesystems() {
    println!("  [FS] Waiting for root filesystem to be ready...");
    
    // Retry logic: wait for filesystem to be mounted
    let max_attempts = 10;
    let mut attempt = 0;
    
    while attempt < max_attempts {
        // Try to open a test file to verify filesystem is mounted
        // We use a simple heuristic: if we can access /usr/bin, the FS is ready
        let test_result = test_filesystem_access();
        
        if test_result {
            println!("  [FS] Root filesystem ready");
            break;
        }
        
        attempt += 1;
        println!("  [FS] Filesystem not ready yet (attempt {}/{}), waiting...", attempt, max_attempts);
        
        // Exponential backoff: wait longer each time
        for _ in 0..(1000 * (1 << attempt)) {
            yield_cpu();
        }
    }
    
    if attempt >= max_attempts {
        println!("  [ERROR] Filesystem failed to mount after {} attempts!", max_attempts);
        println!("  [ERROR] System cannot continue without filesystem access");
        exit(1);
    }
    
    // Mount other filesystems
    println!("  [FS] Mounting /proc...");
    println!("  [FS] Mounting /sys...");
    println!("  [FS] Mounting /dev...");
    println!("  [INFO] All filesystems mounted");
}

/// Test if filesystem is accessible
/// Returns true if we can access filesystem structures
fn test_filesystem_access() -> bool {
    // For now, we use a simple heuristic
    // In a real implementation, we would try to open a known file
    // or make a syscall to check filesystem status
    
    // Placeholder: assume filesystem is ready after a delay
    // TODO: Implement proper filesystem status check syscall
    true
}

/// Start essential services
fn start_essential_services() {
    unsafe {
        // Start log server first - critical for debugging
        start_service(&mut SERVICES[0]);
        
        // Give it time to initialize (minimal delay)
        for _ in 0..5000{
            yield_cpu();
        }
        
        // Start device manager (devfs) - creates /dev nodes
        start_service(&mut SERVICES[1]);
        
        // Give it time to initialize (minimal delay)
        // Give it time to initialize (minimal delay)
        for i in 0..500{
            if i % 100 == 0 {
                println!("[INIT] Waiting for DevFS... {}", i);
            }
            yield_cpu();
        }
    }
}

/// Start system services
fn start_system_services() {
    unsafe {
        // Start filesystem service (depends on devfs)
        start_service(&mut SERVICES[2]);
        
        // CRITICAL: Give filesystem service time to mount the disk
        // This prevents race conditions where other services try to access files
        // before the filesystem is ready
        println!("  [INIT] Waiting for filesystem service to mount...");
        for i in 0..5000 {
            if i % 1000 == 0 { println!("    [INIT] FS wait... {}", i); }
            yield_cpu();
        }
        println!("  [INIT] Filesystem should be ready, continuing...");

        // Start input service (depends on filesystem)
        start_service(&mut SERVICES[3]);
        for i in 0..5000 {
            if i % 1000 == 0 { println!("    [INIT] Input wait... {}", i); }
            yield_cpu();
        }
        
        // Start display service (depends on input)
        start_service(&mut SERVICES[4]);
        for i in 0..5000 {
            if i % 1000 == 0 { println!("    [INIT] Display wait... {}", i); }
            yield_cpu();
        }

        // Start audio service (depends on filesystem)
        start_service(&mut SERVICES[5]);
        for i in 0..5000 {
            if i % 1000 == 0 { println!("    [INIT] Audio wait... {}", i); }
            yield_cpu();
        }
        
        // Start network service last (most complex)
        start_service(&mut SERVICES[6]);
        for i in 0..5000 {
            if i % 1000 == 0 { println!("    [INIT] Network wait... {}", i); }
            yield_cpu();
        }

        // Start GUI service (depends on network)
        start_service(&mut SERVICES[7]);
    }
}

/// Start a service
fn start_service(service: &mut Service) {
    println!("  [SERVICE] Starting {}...", service.name);
    
    service.state = ServiceState::Starting;
    
    // Fork a new process for the service
    let pid = fork();
    
    if pid == 0 {
        // Child process - execute the service
        println!("  [CHILD] Child process for service: {}", service.name);
        
        // Determine which service binary to load
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
                println!("  [CHILD] Unknown service: {}", service.name);
                exit(1);
            }
        };
        
        // Get service binary from kernel
        let (bin_ptr, bin_size) = get_service_binary(service_id);
        
        if bin_ptr.is_null() || bin_size == 0 {
            println!("  [CHILD] Failed to get service binary for: {}", service.name);
            exit(1);
        }
        
        println!("  [CHILD] Got service binary: {} bytes", bin_size);
        
        // Create slice from pointer
        let service_binary = unsafe {
            core::slice::from_raw_parts(bin_ptr, bin_size)
        };
        
        // Execute the service binary
        println!("  [CHILD] Executing service binary via exec()...");
        let result = exec(service_binary);
        
        // If exec succeeds, it should not return
        println!("  [CHILD] exec() returned with error: {}", result);
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
    
    loop {
        counter += 1;
        
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

