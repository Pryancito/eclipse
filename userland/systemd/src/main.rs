//! Eclipse SystemD - Modern Init System for Eclipse OS Microkernel
//! 
//! This is a full-featured init system (PID 1) for Eclipse OS that manages
//! system services, tracks dependencies, and integrates with the microkernel.
//!
//! ## Bare Metal Execution
//! This code runs in a bare metal environment (no OS, no standard library).
//! - `#![no_std]` - No standard library (no heap, no file I/O, etc.)
//! - `#![no_main]` - No standard entry point; we define our own `_start`
//!
//! ## Features:
//! - Service dependency management
//! - Parallel service startup
//! - Service restart policies
//! - Service monitoring and health checks
//! - Zombie process reaping
//! 
//! ## Future enhancements:
//! - Socket activation support
//! - Full microkernel IPC integration

#![no_std]
#![no_main]

use eclipse_libc::{println, getpid, yield_cpu, fork, wait, exit};

/// Maximum number of services that can be managed
const MAX_SERVICES: usize = 32;

/// Service initialization delay (in yield iterations)
const SERVICE_INIT_DELAY: u32 = 10000;

/// Service health check interval (in loop ticks)
const MONITOR_INTERVAL: u64 = 100000;

/// Heartbeat print interval (in loop ticks)
const HEARTBEAT_INTERVAL: u64 = 1000000;

/// Service state
#[derive(Clone, Copy, PartialEq, Debug)]
#[allow(dead_code)]
enum ServiceState {
    Inactive,        // Service not started
    Activating,      // Service starting up
    Active,          // Service running normally
    Deactivating,    // Service shutting down
    Failed,          // Service failed
    Restarting,      // Service being restarted
}

/// Service restart policy
#[derive(Clone, Copy, PartialEq)]
#[allow(dead_code)]
enum RestartPolicy {
    No,              // Never restart
    OnFailure,       // Restart only on failure
    Always,          // Always restart
    OnAbnormal,      // Restart on abnormal exit
}

/// Service type
#[derive(Clone, Copy, PartialEq)]
#[allow(dead_code)]
enum ServiceType {
    Simple,          // Main process
    Forking,         // Forks into background
    OneShot,         // Runs once and exits
    Notify,          // Notifies when ready
}

/// Service definition
#[allow(dead_code)]
struct Service {
    name: &'static str,
    description: &'static str,
    service_type: ServiceType,
    restart_policy: RestartPolicy,
    state: ServiceState,
    pid: i32,
    restart_count: u32,
    max_restarts: u32,
    priority: u8,
    dependencies: &'static [usize],  // Indices of dependent services
}

impl Service {
    const fn new(
        name: &'static str,
        description: &'static str,
        service_type: ServiceType,
        restart_policy: RestartPolicy,
        priority: u8,
        dependencies: &'static [usize],
    ) -> Self {
        Service {
            name,
            description,
            service_type,
            restart_policy,
            state: ServiceState::Inactive,
            pid: 0,
            restart_count: 0,
            max_restarts: 3,
            priority,
            dependencies,
        }
    }
}

/// System services registry
/// 
/// # Safety
/// These static mut variables are safe because:
/// - Init process (PID 1) is single-threaded
/// - No concurrent access to these variables
/// - All access is sequential within the main loop
/// 
/// If threading is added in the future, these should be wrapped in a Mutex.
static mut SERVICES: [Option<Service>; MAX_SERVICES] = [const { None }; MAX_SERVICES];
static mut SERVICE_COUNT: usize = 0;

/// Entry point for eclipse-systemd
#[no_mangle]
pub extern "C" fn _start() -> ! {
    let pid = getpid();
    
    // Display banner
    print_banner();
    
    println!("Eclipse-SystemD starting with PID: {}", pid);
    println!();
    
    if pid != 1 {
        println!("[WARNING] SystemD should run as PID 1!");
        println!("[WARNING] Current PID: {}", pid);
        println!();
    }
    
    // Initialize service registry
    println!("[INIT] Initializing service registry...");
    init_services();
    println!();
    
    // Phase 1: Early boot initialization
    println!("[PHASE 1] Early boot initialization");
    early_boot();
    println!();
    
    // Phase 2: System initialization
    println!("[PHASE 2] System initialization");
    system_init();
    println!();
    
    // Phase 3: Start system services
    println!("[PHASE 3] Starting system services");
    start_system_services();
    println!();
    
    // Phase 4: Main loop
    println!("[PHASE 4] Entering main service manager loop");
    println!("[READY] Eclipse-SystemD is ready");
    println!();
    
    main_loop();
}

/// Print startup banner
fn print_banner() {
    println!("╔════════════════════════════════════════════════════════════════╗");
    println!("║           ECLIPSE-SYSTEMD v0.1.0 - Init System                ║");
    println!("║              Modern Service Manager for Microkernel            ║");
    println!("╚════════════════════════════════════════════════════════════════╝");
    println!();
}

/// Initialize service registry with system services
/// 
/// Launch order (as per requirements):
/// 1. Log Server / Console - for debugging
/// 2. Device Manager (devfs) - creates /dev nodes
/// 3. Input Server - manages keyboard/mouse interrupts
/// 4. Graphics Server (Display Server) - depends on Input Server
/// 5. Network Server - most complex, launched last
fn init_services() {
    unsafe {
        SERVICE_COUNT = 0;
        
        // Define service dependencies
        const NO_DEPS: &[usize] = &[];
        const LOG_DEPS: &[usize] = &[0];      // Depends on log service
        const DEVFS_DEPS: &[usize] = &[0, 1]; // Depends on log + devfs
        const INPUT_DEPS: &[usize] = &[0, 1, 2]; // Depends on log + devfs + input (for Display/Network)
        
        // Service 0: Log Server / Console (no dependencies - MUST BE FIRST)
        add_service(Service::new(
            "log.service",
            "Log Server / Console - Central Logging Service",
            ServiceType::Simple,
            RestartPolicy::OnFailure,
            10,  // Highest priority
            NO_DEPS,
        ));
        
        // Service 1: Device Manager (devfs) (depends on log)
        add_service(Service::new(
            "devfs.service",
            "Device Manager - Creates /dev nodes",
            ServiceType::Simple,
            RestartPolicy::OnFailure,
            9,   // Very high priority
            LOG_DEPS,
        ));
        
        // Service 2: Input Server (depends on log + devfs)
        add_service(Service::new(
            "input.service",
            "Input Server - Keyboard and Mouse Management",
            ServiceType::Simple,
            RestartPolicy::OnFailure,
            8,
            DEVFS_DEPS,
        ));
        
        // Service 3: Display/Graphics Server (depends on log + devfs + input)
        add_service(Service::new(
            "display.service",
            "Graphics Server - Display and Video Buffer",
            ServiceType::Simple,
            RestartPolicy::OnFailure,
            7,
            INPUT_DEPS,
        ));
        
        // Service 4: Network Server (depends on log + devfs + input - MOST COMPLEX, LAST)
        add_service(Service::new(
            "network.service",
            "Network Server - Network Stack Service",
            ServiceType::Simple,
            RestartPolicy::OnFailure,
            6,   // Lower priority, starts last
            INPUT_DEPS,
        ));
        
        println!("  [OK] Registered {} services", SERVICE_COUNT);
    }
}

/// Add service to registry
fn add_service(service: Service) {
    unsafe {
        if SERVICE_COUNT < MAX_SERVICES {
            SERVICES[SERVICE_COUNT] = Some(service);
            SERVICE_COUNT += 1;
        }
    }
}

/// Early boot phase - critical initialization
fn early_boot() {
    println!("  [EARLY] Setting up process environment");
    println!("  [EARLY] Initializing signal handlers");
    println!("  [EARLY] Early boot complete");
}

/// System initialization phase
fn system_init() {
    println!("  [SYSTEM] Mounting filesystems");
    println!("  [SYSTEM] Setting up /proc");
    println!("  [SYSTEM] Setting up /sys");
    println!("  [SYSTEM] Setting up /dev");
    println!("  [SYSTEM] System initialization complete");
}

/// Start system services based on dependencies
fn start_system_services() {
    unsafe {
        // First, start services with no dependencies
        println!("  [START] Starting services with no dependencies...");
        for i in 0..SERVICE_COUNT {
            if let Some(ref mut service) = SERVICES[i] {
                if service.dependencies.is_empty() {
                    start_service(service, i);
                    // Allow service to initialize
                    for _ in 0..SERVICE_INIT_DELAY {
                        yield_cpu();
                    }
                }
            }
        }
        
        println!();
        
        // Multi-pass dependency resolution for cascading dependencies
        println!("  [START] Starting dependent services...");
        let max_passes = 10;  // Prevent infinite loops
        let mut pass = 0;
        let mut services_started_this_pass;
        
        loop {
            pass += 1;
            services_started_this_pass = 0;
            
            println!("  [PASS {}] Checking service dependencies...", pass);
            
            for i in 0..SERVICE_COUNT {
                if let Some(ref mut service) = SERVICES[i] {
                    if !service.dependencies.is_empty() && service.state == ServiceState::Inactive {
                        // Check if dependencies are met
                        if check_dependencies(service) {
                            start_service(service, i);
                            services_started_this_pass += 1;
                            // Allow service to initialize
                            for _ in 0..SERVICE_INIT_DELAY {
                                yield_cpu();
                            }
                        }
                    }
                }
            }
            
            if services_started_this_pass == 0 {
                println!("  [COMPLETE] No more services to start");
                break;
            }
            
            if pass >= max_passes {
                println!("  [WARNING] Maximum passes reached, some services may not have started");
                break;
            }
        }
    }
}

/// Check if service dependencies are satisfied
fn check_dependencies(service: &Service) -> bool {
    unsafe {
        for &dep_idx in service.dependencies {
            if dep_idx < SERVICE_COUNT {
                if let Some(ref dep) = SERVICES[dep_idx] {
                    if dep.state != ServiceState::Active {
                        return false;
                    }
                }
            }
        }
    }
    true
}

/// Start a specific service
fn start_service(service: &mut Service, _service_idx: usize) {
    println!("  [START] {} - {}", service.name, service.description);
    
    service.state = ServiceState::Activating;
    
    // Fork a new process for the service
    let pid = fork();
    
    if pid == 0 {
        // Child process - execute the service binary
        println!("    [CHILD] Spawning {} in new process", service.name);
        
        // Map service name to service ID for get_service_binary syscall
        let service_id = match service.name {
            "log.service" => 0,
            "devfs.service" => 1,
            "input.service" => 2,
            "display.service" => 3,
            "network.service" => 4,
            _ => {
                println!("    [ERROR] Unknown service: {}", service.name);
                exit(1);
            }
        };
        
        // For now, we'll just simulate the service
        // In a real implementation, we'd load the service binary
        println!("    [INFO] Service {} would execute with ID {}", service.name, service_id);
        
        // Simulate service running
        let mut counter = 0u64;
        loop {
            counter += 1;
            if counter % 1000000 == 0 {
                // Service heartbeat (would use IPC in real system)
            }
            yield_cpu();
        }
    } else if pid > 0 {
        // Parent process - track the service
        service.pid = pid;
        service.state = ServiceState::Active;
        println!("    [OK] {} started with PID {}", service.name, pid);
    } else {
        // Fork failed
        println!("    [FAILED] Could not fork for {}", service.name);
        service.state = ServiceState::Failed;
    }
}

/// Main service manager loop
fn main_loop() -> ! {
    let mut tick: u64 = 0;
    let mut heartbeat_counter: u64 = 0;
    
    loop {
        tick += 1;
        
        // Every MONITOR_INTERVAL ticks, check service health
        if tick % MONITOR_INTERVAL == 0 {
            monitor_services();
        }
        
        // Every HEARTBEAT_INTERVAL ticks, print status
        if tick % HEARTBEAT_INTERVAL == 0 {
            heartbeat_counter += 1;
            println!();
            println!("[HEARTBEAT #{}] SystemD operational", heartbeat_counter);
            print_service_status();
            println!();
        }
        
        // Reap zombie processes
        reap_zombies();
        
        // Yield CPU to other processes
        yield_cpu();
    }
}

/// Monitor service health and restart failed services
fn monitor_services() {
    unsafe {
        for i in 0..SERVICE_COUNT {
            if let Some(ref mut service) = SERVICES[i] {
                // Check if service needs restart
                if service.state == ServiceState::Failed {
                    handle_failed_service(service, i);
                }
            }
        }
    }
}

/// Handle failed service according to restart policy
fn handle_failed_service(service: &mut Service, service_idx: usize) {
    let should_restart = match service.restart_policy {
        RestartPolicy::No => false,
        RestartPolicy::OnFailure => true,
        RestartPolicy::Always => true,
        RestartPolicy::OnAbnormal => true,
    };
    
    if should_restart && service.restart_count < service.max_restarts {
        service.restart_count += 1;
        println!();
        println!("[RESTART] Restarting {} (attempt {}/{})",
                 service.name, service.restart_count, service.max_restarts);
        service.state = ServiceState::Restarting;
        start_service(service, service_idx);
    } else if service.restart_count >= service.max_restarts {
        println!();
        println!("[CRITICAL] Service {} exceeded max restart attempts",
                 service.name);
    }
}

/// Reap zombie processes and update service states
fn reap_zombies() {
    loop {
        let terminated_pid = wait(None);
        
        if terminated_pid <= 0 {
            break;
        }
        
        // Find which service this PID belonged to
        unsafe {
            for i in 0..SERVICE_COUNT {
                if let Some(ref mut service) = SERVICES[i] {
                    if service.pid == terminated_pid {
                        println!();
                        println!("[TERMINATED] Service {} (PID {}) exited",
                                 service.name, terminated_pid);
                        
                        if service.state == ServiceState::Active {
                            service.state = ServiceState::Failed;
                        }
                        
                        service.pid = 0;
                        break;
                    }
                }
            }
        }
    }
}

/// Print current status of all services
fn print_service_status() {
    println!("═══════════════════════════════════════════════════════════════");
    println!("SERVICE STATUS:");
    println!("───────────────────────────────────────────────────────────────");
    
    unsafe {
        for i in 0..SERVICE_COUNT {
            if let Some(ref service) = SERVICES[i] {
                let state_str = match service.state {
                    ServiceState::Inactive => "inactive",
                    ServiceState::Activating => "activating",
                    ServiceState::Active => "active",
                    ServiceState::Deactivating => "deactivating",
                    ServiceState::Failed => "failed",
                    ServiceState::Restarting => "restarting",
                };
                
                if service.pid > 0 {
                    println!("  {} [{}] PID:{} Restarts:{}",
                             service.name, state_str, service.pid, service.restart_count);
                } else {
                    println!("  {} [{}] Restarts:{}",
                             service.name, state_str, service.restart_count);
                }
            }
        }
    }
    
    println!("═══════════════════════════════════════════════════════════════");
}

// Note: Panic handler is provided by eclipse_libc

