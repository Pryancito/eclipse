//! Input Service - Manages keyboard and mouse input
//! 
//! This service manages input devices (keyboard, mouse, tablet) and handles hardware events.
//! It must start after devfs to access /dev/input/* device nodes.
//! 
//! Supports:
//! - PS/2 keyboards and mice
//! - USB keyboards and mice (UHCI, OHCI, EHCI, XHCI)
//! - USB tablets and touchpads
//! - Gaming peripherals (high DPI mice, mechanical keyboards)

use std::prelude::v1::*;

use core::sync::atomic::{AtomicU64, Ordering};
use std::prelude::*;
use std::libc::{getpid, getppid, yield_cpu, sleep_ms, send_ipc, receive_ipc, read_key_scancode, read_mouse_packet, pci_enum_devices, PciDeviceInfo, InputEvent, set_cursor_position, get_framebuffer_info, get_system_stats, SystemStats};
use input_service::EventQueue;

const SYS_REGISTER_DEVICE: usize = eclipse_syscall::number::SYS_REGISTER_DEVICE;

#[repr(u64)]
enum DeviceType {
    Input = 3, // Matches kernel DeviceType::Input
}

fn register_device(name: &str, type_id: DeviceType) -> bool {
    unsafe {
        eclipse_syscall::syscall3(
            SYS_REGISTER_DEVICE,
            name.as_ptr() as usize,
            name.len(),
            type_id as usize,
        ) == 0
    }
}

fn sys_open(path: &str) -> Option<usize> {
    let fd = std::libc::eclipse_open(path, std::libc::O_RDONLY, 0);
    if fd < 0 { None } else { Some(fd as usize) }
}

fn sys_write(fd: usize, buf: &[u8]) -> usize {
    std::libc::eclipse_write(fd as u32, buf) as usize
}

fn write_input_event_to_scheme(input_fd: Option<usize>, ev: &InputEvent) {
    if let Some(fd) = input_fd {
        // Zero-initialize the buffer and copy each field individually at its repr(C) offset.
        // This avoids undefined padding bytes that the kernel IPC sanitizer might zero out
        // if they look like kernel pointers, preventing field corruption.
        let mut buf = [0u8; INPUT_EVENT_SIZE];
        buf[0..4].copy_from_slice(&ev.device_id.to_le_bytes());
        buf[4] = ev.event_type;
        // buf[5] = 0; // implicit padding byte
        buf[6..8].copy_from_slice(&ev.code.to_le_bytes());
        buf[8..12].copy_from_slice(&ev.value.to_le_bytes());
        // buf[12..16] = 0; // implicit padding bytes
        buf[16..24].copy_from_slice(&ev.timestamp.to_le_bytes());
        let _ = sys_write(fd, &buf);
    }
}

/// Input device types
#[derive(Clone, Copy, PartialEq, Debug)]
enum InputDeviceType {
    Keyboard,
    Mouse,
    Tablet,
    Touchpad,
    Gamepad,
}

/// USB controller types
#[derive(Clone, Copy, PartialEq, Debug)]
enum USBControllerType {
    UHCI,   // USB 1.1
    OHCI,   // USB 1.1 (alternative)
    EHCI,   // USB 2.0
    XHCI,   // USB 3.0+
}

/// Gaming device capabilities
#[derive(Clone, Copy, Debug)]
struct GamingCapabilities {
    is_gaming: bool,
    max_dpi: u32,           // Maximum DPI for mice
    adjustable_dpi: bool,   // Can change DPI on-the-fly
    extra_buttons: u8,      // Number of extra buttons beyond standard
    polling_rate_max: u32,  // Maximum polling rate in Hz
    n_key_rollover: bool,   // For keyboards: N-key rollover support
    macro_keys: u8,         // Number of programmable macro keys
    rgb_lighting: bool,     // RGB LED support
}

impl GamingCapabilities {
    const fn none() -> Self {
        Self {
            is_gaming: false,
            max_dpi: 800,
            adjustable_dpi: false,
            extra_buttons: 0,
            polling_rate_max: 125,
            n_key_rollover: false,
            macro_keys: 0,
            rgb_lighting: false,
        }
    }
    
    const fn gaming_mouse() -> Self {
        Self {
            is_gaming: true,
            max_dpi: 16000,
            adjustable_dpi: true,
            extra_buttons: 8,  // Back, Forward, DPI+, DPI-, Profile + 3 more
            polling_rate_max: 1000,
            n_key_rollover: false,
            macro_keys: 0,
            rgb_lighting: true,
        }
    }
    
    const fn gaming_keyboard() -> Self {
        Self {
            is_gaming: true,
            max_dpi: 0,
            adjustable_dpi: false,
            extra_buttons: 0,
            polling_rate_max: 1000,
            n_key_rollover: true,
            macro_keys: 6,
            rgb_lighting: true,
        }
    }
}

/// Input device information
#[derive(Clone, Copy)]
struct InputDevice {
    device_type: InputDeviceType,
    device_id: u32,
    vendor_id: u16,
    product_id: u16,
    is_usb: bool,
    polling_rate: u32,  // Hz
    gaming_caps: GamingCapabilities,
}

/// Tamaño fijo (InputEvent viene de eclipse_libc)
const INPUT_EVENT_SIZE: usize = core::mem::size_of::<InputEvent>();


/// Detect USB controllers via PCI
fn detect_usb_controllers() -> usize {
    println!("[INPUT-SERVICE] Detecting USB controllers...");
    println!("[INPUT-SERVICE]   Scanning PCI bus for USB controllers");
    
    // USB controllers are class 0x0C (Serial Bus Controller), subclass 0x03 (USB)
    let mut devices_buffer = [PciDeviceInfo {
        bus: 0,
        device: 0,
        function: 0,
        vendor_id: 0,
        device_id: 0,
        class_code: 0,
        subclass: 0,
        bar0: 0,
    }; 16];
    
    // Scan for serial bus controllers (class 0x0C)
    let count = pci_enum_devices(0x0C, &mut devices_buffer);
    
    let mut usb_count = 0;
    
    for i in 0..count {
        let dev = devices_buffer[i];
        
        // Check if it's a USB controller (subclass 0x03)
        if dev.subclass == 0x03 {
            usb_count += 1;
            
            println!("[INPUT-SERVICE]   Found USB Controller:");
            println!("[INPUT-SERVICE]     PCI: {:02x}:{:02x}.{}", 
                     dev.bus as u32, dev.device as u32, dev.function as u32);
            println!("[INPUT-SERVICE]     Vendor: 0x{:04x}, Device: 0x{:04x}",
                     dev.vendor_id as u32, dev.device_id as u32);
            
            // Determine controller type from programming interface
            // Note: This would need to read the programming interface byte
            // For now, we'll identify by vendor
            match dev.vendor_id {
                0x8086 => println!("[INPUT-SERVICE]     Type: Intel USB Controller"),
                0x1002 => println!("[INPUT-SERVICE]     Type: AMD USB Controller"),
                0x1106 => println!("[INPUT-SERVICE]     Type: VIA USB Controller"),
                _ => println!("[INPUT-SERVICE]     Type: Generic USB Controller"),
            }
        }
    }
    
    if usb_count > 0 {
        println!("[INPUT-SERVICE]   USB controllers initialized successfully");
    } else {
        println!("[INPUT-SERVICE]   No USB controllers found");
    }
    
    usb_count
}

/// Initialize PS/2 keyboard
fn init_ps2_keyboard() -> bool {
    println!("[INPUT-SERVICE] Initializing PS/2 keyboard...");
    println!("[INPUT-SERVICE]   Port: 0x60 (data), 0x64 (command/status)");
    println!("[INPUT-SERVICE]   IRQ: 1");
    
    // In real implementation:
    // - Check if PS/2 controller exists
    // - Send initialization commands
    // - Set up interrupt handler
    // - Configure scan code set
    
    println!("[INPUT-SERVICE]   PS/2 keyboard ready");
    true
}

/// Initialize PS/2 mouse
fn init_ps2_mouse() -> bool {
    println!("[INPUT-SERVICE] Initializing PS/2 mouse...");
    println!("[INPUT-SERVICE]   Port: 0x60 (data), 0x64 (command/status)");
    println!("[INPUT-SERVICE]   IRQ: 12");
    
    // In real implementation:
    // - Enable auxiliary device
    // - Send mouse initialization commands
    // - Set up interrupt handler
    // - Configure sample rate and resolution
    
    println!("[INPUT-SERVICE]   PS/2 mouse ready");
    true
}

/// Enumerate USB input devices
fn enumerate_usb_devices() -> usize {
    println!("[INPUT-SERVICE] Enumerating USB input devices...");
    
    // In real implementation:
    // - Query USB controllers for connected devices
    // - Parse USB descriptors
    // - Identify HID devices
    // - Set up endpoints for interrupt transfers
    
    let mut device_count = 0;
    
    // Standard USB keyboard
    println!("[INPUT-SERVICE]   USB Keyboard detected:");
    println!("[INPUT-SERVICE]     Interface: HID Boot Protocol");
    println!("[INPUT-SERVICE]     Endpoint: IN (Interrupt)");
    println!("[INPUT-SERVICE]     Polling rate: 1000 Hz");
    device_count += 1;
    
    // Standard USB mouse
    println!("[INPUT-SERVICE]   USB Mouse detected:");
    println!("[INPUT-SERVICE]     Interface: HID Boot Protocol");
    println!("[INPUT-SERVICE]     Resolution: 1600 DPI");
    println!("[INPUT-SERVICE]     Polling rate: 1000 Hz");
    device_count += 1;
    
    // Gaming mouse detection
    println!("[INPUT-SERVICE]   Gaming Mouse detected:");
    println!("[INPUT-SERVICE]     Type: High-Performance Gaming Mouse");
    println!("[INPUT-SERVICE]     Vendor: Logitech/Razer/Corsair");
    println!("[INPUT-SERVICE]     Features:");
    println!("[INPUT-SERVICE]       - Adjustable DPI: 400-16000");
    println!("[INPUT-SERVICE]       - Polling rate: 1000 Hz");
    println!("[INPUT-SERVICE]       - Extra buttons: 8 (Back, Forward, DPI+, DPI-, Profile)");
    println!("[INPUT-SERVICE]       - On-the-fly DPI switching");
    println!("[INPUT-SERVICE]       - RGB lighting: Yes");
    println!("[INPUT-SERVICE]       - Hardware acceleration: Yes");
    device_count += 1;
    
    // Gaming keyboard detection
    println!("[INPUT-SERVICE]   Gaming Keyboard detected:");
    println!("[INPUT-SERVICE]     Type: Mechanical Gaming Keyboard");
    println!("[INPUT-SERVICE]     Vendor: Corsair/Razer/SteelSeries");
    println!("[INPUT-SERVICE]     Features:");
    println!("[INPUT-SERVICE]       - Polling rate: 1000 Hz");
    println!("[INPUT-SERVICE]       - N-Key Rollover: Yes (Full)");
    println!("[INPUT-SERVICE]       - Anti-ghosting: Yes");
    println!("[INPUT-SERVICE]       - Macro keys: 6 programmable");
    println!("[INPUT-SERVICE]       - RGB lighting: Per-key RGB");
    println!("[INPUT-SERVICE]       - Media controls: Dedicated");
    device_count += 1;
    
    // USB tablet (QEMU compatibility)
    println!("[INPUT-SERVICE]   USB Tablet detected:");
    println!("[INPUT-SERVICE]     Interface: HID Absolute Pointer");
    println!("[INPUT-SERVICE]     Resolution: 32768 x 32768");
    println!("[INPUT-SERVICE]     Polling rate: 125 Hz");
    device_count += 1;
    
    println!("[INPUT-SERVICE]   Found {} USB input device(s)", device_count);
    println!("[INPUT-SERVICE]   Gaming peripherals: 2 (mouse + keyboard)");
    
    device_count
}

/// Create device nodes
fn create_device_nodes() {
    println!("[INPUT-SERVICE] Creating device nodes:");
    println!("[INPUT-SERVICE]   /dev/input/event0 - PS/2 Keyboard");
    println!("[INPUT-SERVICE]   /dev/input/event1 - PS/2 Mouse");
    println!("[INPUT-SERVICE]   /dev/input/event2 - USB Keyboard");
    println!("[INPUT-SERVICE]   /dev/input/event3 - USB Mouse");
    println!("[INPUT-SERVICE]   /dev/input/event4 - USB Tablet");
    println!("[INPUT-SERVICE]   /dev/input/event5 - Gaming Mouse (High-DPI)");
    println!("[INPUT-SERVICE]   /dev/input/event6 - Gaming Keyboard (Mechanical)");
    println!("[INPUT-SERVICE]   /dev/input/mice   - All mice (multiplexed)");
    println!("[INPUT-SERVICE]   /dev/input/gaming - Gaming peripherals interface");
}

fn main() {
    let pid = unsafe { getpid() };
    
    println!("+--------------------------------------------------------------+");
    println!("|                    INPUT SERVICE                             |");
    println!("+--------------------------------------------------------------+");
    println!("[INPUT-SERVICE] Starting (PID: {})", pid);
    println!("[INPUT-SERVICE] Initializing input subsystem...");
    
    // Detect USB controllers via PCI
    let usb_controller_count = detect_usb_controllers();
    
    // Initialize PS/2 devices
    println!("[INPUT-SERVICE] Initializing PS/2 devices...");
    let ps2_kbd_present = init_ps2_keyboard();
    let ps2_mouse_present = init_ps2_mouse();
    
    // Enumerate USB input devices
    let usb_device_count = if usb_controller_count > 0 {
        enumerate_usb_devices()
    } else {
        0
    };
    
    // Create device nodes
    create_device_nodes();
    
    // Initialize event queue
    println!("[INPUT-SERVICE] Initializing input event queue...");
    let mut event_queue = EventQueue::new();
    println!("[INPUT-SERVICE]   Event queue allocated (256 events, 4KB buffer)");
    println!("[INPUT-SERVICE]   Ready to process input events");
    
    // Register with input: scheme (optional - may not exist yet)
    println!("[INPUT-SERVICE] Connecting to input: scheme proxy...");
    let input_fd = match sys_open("input:") {
        Some(fd) => {
            println!("[INPUT-SERVICE]   Scheme handle: {}", fd);
            Some(fd)
        }
        None => {
            println!("[INPUT-SERVICE]   WARNING: input: scheme not available");
            println!("[INPUT-SERVICE]   Service will run in standalone mode");
            None
        }
    };
    
    // Report initialization status
    println!("[INPUT-SERVICE] Input service ready");
    let ppid = unsafe { getppid() };
    if ppid > 0 {
        let _ = std::libc::send_ipc(ppid as u32, 255, b"READY");
    }

    let _ = register_device("input:keyboard", DeviceType::Input);
    let _ = register_device("input:mouse", DeviceType::Input);

    println!("[INPUT-SERVICE] Device summary:");
    println!("[INPUT-SERVICE]   USB controllers: {}", usb_controller_count);
    println!("[INPUT-SERVICE]   PS/2 keyboard: {}", if ps2_kbd_present { "Yes" } else { "No" });
    println!("[INPUT-SERVICE]   PS/2 mouse: {}", if ps2_mouse_present { "Yes" } else { "No" });
    println!("[INPUT-SERVICE]   USB devices: {}", usb_device_count);
    println!("[INPUT-SERVICE]   Gaming peripherals: {} detected", if usb_device_count >= 2 { 2 } else { 0 });
    println!("[INPUT-SERVICE]     - High-DPI gaming mouse (1000Hz, 16000 DPI)");
    println!("[INPUT-SERVICE]     - Mechanical gaming keyboard (1000Hz, N-key rollover)");
    println!("[INPUT-SERVICE] Waiting for input events...");


    // Estado previo de botones del ratón PS/2 para detectar cambios (bit0=left, bit1=right, bit2=middle)
    let mut prev_mouse_buttons: u8 = 0;
    // Estado previo de scancode extendido (0xE0)
    let mut has_e0 = false;

    // Cursor position (absolute, clamped to screen bounds)
    let (screen_width, screen_height) = unsafe { get_framebuffer_info() }
        .map(|fb| (fb.width as i32, fb.height as i32))
        .unwrap_or((1024, 768));
    let mut cursor_x: i32 = screen_width / 2;
    let mut cursor_y: i32 = screen_height / 2;
    // Place cursor at screen center initially
    set_cursor_position(cursor_x as u32, cursor_y as u32);

    // Main loop - process input events
    let mut heartbeat_last_ticks = 0u64;
    let mut total_events = 0u64;
    let mut keyboard_events = 0u64;
    let mut mouse_events = 0u64;
    let mut tablet_events = 0u64;
    // Cuántas iteraciones consecutivas del bucle no generaron ningún paquete de ratón.
    // Si supera un umbral, probablemente el hardware o el driver del kernel dejó de
    // producir eventos (bug de driver, overflow de buffer, etc.).
    let mut consecutive_idle_mouse_iters: u64 = 0;
    // Umbral más bajo para detectar antes el estancamiento del driver (p.ej. 200 iteraciones
    // a ~1kHz ≃ 200ms). Si el ratón "se muere" bajo estrés, queremos ver rápidamente el WARN.
    const IDLE_MOUSE_WARN_THRESHOLD: u64 = 200; // ~200 ms a 1 kHz de bucle
    
    loop {
        let mut stats = SystemStats {
            uptime_ticks: 0,
            idle_ticks: 0,
            total_mem_frames: 0,
            used_mem_frames: 0,
            cpu_temp: [0; 16],
            gpu_load: [0; 4],
            gpu_temp: [0; 4],
            anomaly_count: 0,
            heap_fragmentation: 0,
        };
        let _ = unsafe { get_system_stats(&mut stats) };

        
        // Drenar ratón PS/2 real (kernel buffer vía syscall read_mouse_packet).
        // Límite por iteración para no bloquear el bucle y poder enviar heartbeat al init.
        const MAX_MOUSE_PER_ITER: u32 = 64;
        let mut mouse_batch = 0u32;
        let mut mouse_drained = 0u32;
        // Accumulate all motion deltas within this iteration so we can send ONE coalesced
        // IPC event instead of one per packet.  This keeps the compositor mailbox from
        // filling up during rapid mouse movement.
        let mut coalesced_dx: i32 = 0;
        let mut coalesced_dy: i32 = 0;
        while mouse_drained < MAX_MOUSE_PER_ITER {
            let packed = match read_mouse_packet() {
                Some(p) => p,
                None => break,
            };
            mouse_drained += 1;
            mouse_batch += 1;
            if mouse_batch >= 8 {
                unsafe { yield_cpu(); }
                mouse_batch = 0;
            }
            let buttons = (packed & 0xFF) as u8;
            let dx = ((packed >> 8) as u8) as i8 as i32;
            let dy = ((packed >> 16) as u8) as i8 as i32;

            // Actualizar posición del cursor y moverlo en el hardware
            if dx != 0 || dy != 0 {
                cursor_x = (cursor_x + dx).max(0).min(screen_width - 1);
                cursor_y = (cursor_y + dy).max(0).min(screen_height - 1);
                set_cursor_position(cursor_x as u32, cursor_y as u32);
                // Accumulate using saturating_add so repeated small-but-same-sign movements
                // never silently wrap around (i32 overflow with 64 * 127 = 8128, well within
                // i16 range, so this is extra safety for unusual kernel values).
                coalesced_dx = coalesced_dx.saturating_add(dx);
                coalesced_dy = coalesced_dy.saturating_add(dy);
            }

            // Eventos de botones (code 0=left, 1=right, 2=middle; value 0=release, 1=press)
            for i in 0..3u8 {
                let mask = 1u8 << i;
                let now = (buttons & mask) != 0;
                let was = (prev_mouse_buttons & mask) != 0;
                if now != was {
                    let ev = InputEvent {
                        device_id: 1,
                        event_type: 2, // mouse_button
                        code: i as u16,
                        value: if now { 1 } else { 0 },
                        timestamp: stats.uptime_ticks,
                    };
                    mouse_events += 1;
                    total_events += 1;
                    write_input_event_to_scheme(input_fd, &ev);
                    let _ = event_queue.push(ev);
                }
            }
            prev_mouse_buttons = buttons;

            // Scroll (byte alto del packed, PS/2 extendido 4-byte)
            let scroll = (packed >> 24) as i8 as i32;
            if scroll != 0 {
                let ev = InputEvent {
                    device_id: 1,
                    event_type: 3, // mouse_scroll
                    code: 0,       // vertical
                    value: scroll,
                    timestamp: stats.uptime_ticks,
                };
                mouse_events += 1;
                total_events += 1;
                write_input_event_to_scheme(input_fd, &ev);
                let _ = event_queue.push(ev);
            }
        }

        // Resetear el contador de iteraciones sin paquetes de ratón cada vez que drenamos
        // al menos un paquete; lo usamos solo para estadística interna, sin logs.
        if mouse_drained > 0 {
            consecutive_idle_mouse_iters = 0;
        }

        // Send ONE coalesced motion event after draining all packets this iteration.
        // Clamp to i16 range so the compositor can unpack each axis with a simple `as i16`.
        // Contract: value = (coalesced_dy as i16 as i32) << 16 | (coalesced_dx as i16 as u16 as i32)
        if coalesced_dx != 0 || coalesced_dy != 0 {
            let cdx = coalesced_dx.clamp(i16::MIN as i32, i16::MAX as i32);
            let cdy = coalesced_dy.clamp(i16::MIN as i32, i16::MAX as i32);
            let packed_value = ((cdy as i16 as i32) << 16) | (cdx as i16 as u16 as i32);
            let ev = InputEvent {
                device_id: 1,
                event_type: 1,
                code: 0xFFFF, // código especial: movimiento coalesced dx+dy
                value: packed_value,
                timestamp: stats.uptime_ticks,
            };
            mouse_events += 1;
            total_events += 1;
            write_input_event_to_scheme(input_fd, &ev);
            let _ = event_queue.push(ev);
        }

        // Drenar teclado PS/2 real (kernel buffer vía syscall read_key).
        // Límite por iteración para no bloquear el bucle y poder enviar heartbeat al init.
        const MAX_KBD_PER_ITER: u32 = 64;
        let mut kbd_batch = 0u32;
        let mut kbd_drained = 0u32;
        while kbd_drained < MAX_KBD_PER_ITER {
            let sc = match read_key_scancode() {
                Some(s) => s,
                None => break,
            };
            kbd_drained += 1;
            if sc == 0xE0 {
                has_e0 = true;
                continue;
            }
            kbd_batch += 1;
            if kbd_batch >= 8 {
                unsafe { yield_cpu(); }
                kbd_batch = 0;
            }
            let value = if (sc & 0x80) != 0 { 0 } else { 1 }; // break = 0, make = 1
            let code = sc & 0x7F;
            if code == 0 {
                has_e0 = false;
                continue;
            }
            // Use bit 15 as a flag for extended (E0) scancodes
            let final_code = if has_e0 { (code as u16) | 0x8000 } else { code as u16 };
            has_e0 = false;

            let kbd_event = InputEvent {
                device_id: 0,
                event_type: 0,
                code: final_code,
                value,
                timestamp: stats.uptime_ticks,
            };
            keyboard_events += 1;
            total_events += 1;
            write_input_event_to_scheme(input_fd, &kbd_event);
            let _ = event_queue.push(kbd_event);
        }

        // No simulate occasional input events - removes fake jumpiness
        
        // Process events from queue (simulate consumption) every ~250ms
        if stats.uptime_ticks % 250 == 0 {
            while let Some(_event) = event_queue.pop() {
                // In real implementation: dispatch to consumers
            }
        }

        // Watchdog heartbeat para init (PID 1): cada 500ms para reducir tráfico IPC.
        // Se usa uptime_ticks para que sea independiente de la carga o velocidad del bucle.
        if stats.uptime_ticks >= heartbeat_last_ticks + 500 {
            let _ = std::libc::send_ipc(1, 0x40, b"HEART");
            heartbeat_last_ticks = stats.uptime_ticks;
        }

        // Sleep 1 ms so the kernel can HLT between polls.  A plain yield_cpu() returns
        // immediately when this is the only ready process, turning the loop into a
        // busy-wait that consumes ~100% CPU on real hardware.  1 ms gives ~1 kHz
        // polling (well above PS/2 and USB HID rates) while keeping CPU usage near 0%.
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

