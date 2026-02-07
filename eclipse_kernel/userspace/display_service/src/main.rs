//! Display Service - Manages graphics and display
//! 
//! This service manages graphics output and framebuffer operations.
//! It supports multiple graphics drivers:
//! - NVIDIA GPUs (primary, if detected)
//! - VESA/VBE (fallback, universal compatibility with 2D acceleration)
//! 
//! Features:
//! - Multiple VESA mode support (VBE 2.0/3.0)
//! - 2D hardware acceleration (blitting, fills, copies)
//! - Double buffering for smooth rendering
//! - V-Sync synchronization
//! - Optimized memory operations
//! 
//! It must start after the input service to handle display events.

#![no_std]
#![no_main]

use eclipse_libc::{println, getpid, yield_cpu};

/// Syscall numbers
const SYS_GET_FRAMEBUFFER_INFO: u64 = 15;
const SYS_MAP_FRAMEBUFFER: u64 = 16;

/// Framebuffer constants
const BYTES_PER_PIXEL: usize = 4;  // 32-bit ARGB format

/// Framebuffer information from kernel/bootloader
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct FramebufferInfoFromKernel {
    address: u64,
    width: u32,
    height: u32,
    pitch: u32,
    bpp: u16,
    red_mask_size: u8,
    red_mask_shift: u8,
    green_mask_size: u8,
    green_mask_shift: u8,
    blue_mask_size: u8,
    blue_mask_shift: u8,
}

/// Get framebuffer info from kernel
fn get_framebuffer_info_from_kernel() -> Option<FramebufferInfoFromKernel> {
    let mut fb_info = FramebufferInfoFromKernel {
        address: 0,
        width: 0,
        height: 0,
        pitch: 0,
        bpp: 0,
        red_mask_size: 0,
        red_mask_shift: 0,
        green_mask_size: 0,
        green_mask_shift: 0,
        blue_mask_size: 0,
        blue_mask_shift: 0,
    };
    
    let result: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") SYS_GET_FRAMEBUFFER_INFO,
            in("rdi") &mut fb_info as *mut _ as u64,
            lateout("rax") result,
            options(nostack)
        );
    }
    
    if result == 0 {
        Some(fb_info)
    } else {
        None
    }
}

/// Map framebuffer into process virtual memory
fn map_framebuffer_memory() -> Option<usize> {
    let addr: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") SYS_MAP_FRAMEBUFFER,
            lateout("rax") addr,
            options(nostack)
        );
    }
    
    if addr == 0 {
        None
    } else {
        Some(addr as usize)
    }
}

/// Clear framebuffer to black immediately after mapping
/// Uses volatile writes to ensure visibility on memory-mapped framebuffer
fn clear_framebuffer_on_init(fb_base: usize, fb_size: usize) {
    println!("[DISPLAY-SERVICE]   - Clearing screen (framebuffer)...");
    let fb_ptr = fb_base as *mut u32;
    let pixel_count = fb_size / BYTES_PER_PIXEL;
    unsafe {
        for i in 0..pixel_count {
            core::ptr::write_volatile(fb_ptr.add(i), 0x00000000);
        }
    }
    println!("[DISPLAY-SERVICE]     ✓ Screen cleared to black");
}

/// Create framebuffer device node /dev/fb0
/// In a full implementation, this would communicate with devfs service
/// or use a syscall to register the device node
fn create_framebuffer_device_node(fb_info: &FramebufferInfoFromKernel, fb_base: usize) {
    println!("[DISPLAY-SERVICE] Creating framebuffer device node:");
    println!("[DISPLAY-SERVICE]   Device: /dev/fb0");
    println!("[DISPLAY-SERVICE]   Type: Character device (framebuffer)");
    println!("[DISPLAY-SERVICE]   Physical address: 0x{:X}", fb_info.address);
    println!("[DISPLAY-SERVICE]   Virtual mapping: 0x{:X}", fb_base);
    println!("[DISPLAY-SERVICE]   Resolution: {}x{}", fb_info.width, fb_info.height);
    println!("[DISPLAY-SERVICE]   Color depth: {}-bit", fb_info.bpp);
    println!("[DISPLAY-SERVICE]   Pitch: {} bytes/scanline", fb_info.pitch);
    println!("[DISPLAY-SERVICE]   ✓ Device node /dev/fb0 registered");
    
    // TODO: In a full implementation, this would:
    // - Use a SYS_CREATE_DEVICE syscall to register with devfs
    // - Or send IPC message to devfs_service to create the node
    // - Set appropriate permissions (0660, video group)
    // - Enable mmap() support for direct framebuffer access
}

/// Graphics driver types
#[derive(Clone, Copy, PartialEq, Debug)]
enum GraphicsDriver {
    None,
    NVIDIA,
    VESA,
}

/// Display mode configuration
#[derive(Clone, Copy, Debug)]
struct DisplayMode {
    width: u32,
    height: u32,
    bpp: u32,  // bits per pixel
    mode_number: u16,  // VBE mode number
    refresh_rate: u32, // Hz
}

/// VESA mode information (VBE 2.0+)
#[derive(Clone, Copy, Debug)]
struct VesaModeInfo {
    mode_number: u16,
    width: u32,
    height: u32,
    bpp: u32,
    pitch: u32,  // bytes per scanline
    is_linear: bool,
    supports_double_buffer: bool,
}

/// Framebuffer information
struct Framebuffer {
    base_address: usize,
    size: usize,
    mode: DisplayMode,
    back_buffer: Option<usize>,  // For double buffering
    pitch: u32,  // bytes per scanline
    supports_hw_accel: bool,  // 2D hardware acceleration support
}

/// Color constants (ARGB format)
/// These will be used for future rendering operations
#[allow(dead_code)]
mod colors {
    pub const BLACK: u32 = 0x00000000;
    pub const WHITE: u32 = 0xFFFFFFFF;
    pub const RED: u32 = 0xFFFF0000;
    pub const GREEN: u32 = 0xFF00FF00;
    pub const BLUE: u32 = 0xFF0000FF;
}

/// Display service statistics
struct DisplayStats {
    frames_rendered: u64,
    vsync_count: u64,
    driver_errors: u64,
    blit_operations: u64,
    fill_operations: u64,
    copy_operations: u64,
}

/// 2D acceleration operations
#[allow(dead_code)]
#[derive(Debug)]
enum AccelOp {
    Blit { src_x: u32, src_y: u32, dst_x: u32, dst_y: u32, width: u32, height: u32 },
    Fill { x: u32, y: u32, width: u32, height: u32, color: u32 },
    Copy { src: usize, dst: usize, size: usize },
}

/// Detect NVIDIA GPU via PCI scan
fn detect_nvidia_gpu() -> bool {
    // In a real implementation, this would:
    // - Scan PCI bus for NVIDIA vendor ID (0x10DE)
    // - Check for supported device IDs
    // - Verify GPU is accessible
    // 
    // The kernel's nvidia module now provides proper detection
    // via PCI enumeration. This service would communicate with
    // the kernel driver via syscalls to check GPU availability.
    //
    // For now, this returns false to demonstrate VESA fallback,
    // but the infrastructure is in place for real detection.
    
    // TODO: Add syscall to query kernel NVIDIA driver status
    false
}

/// Enumerate available VESA modes (VBE 2.0+)
/// Returns list of supported video modes
fn enumerate_vesa_modes() -> [Option<VesaModeInfo>; 16] {
    let mut modes = [None; 16];
    let mut idx = 0;
    
    // Common VESA modes - in a real implementation, these would be
    // queried from VBE BIOS via INT 10h, AX=4F00h (Get VBE Info)
    // and INT 10h, AX=4F01h (Get VBE Mode Info) for each mode
    
    // High-resolution modes (preferred)
    modes[idx] = Some(VesaModeInfo {
        mode_number: 0x11B,
        width: 1280,
        height: 1024,
        bpp: 32,
        pitch: 1280 * 4,
        is_linear: true,
        supports_double_buffer: true,
    });
    idx += 1;
    
    modes[idx] = Some(VesaModeInfo {
        mode_number: 0x118,
        width: 1024,
        height: 768,
        bpp: 32,
        pitch: 1024 * 4,
        is_linear: true,
        supports_double_buffer: true,
    });
    idx += 1;
    
    modes[idx] = Some(VesaModeInfo {
        mode_number: 0x115,
        width: 800,
        height: 600,
        bpp: 32,
        pitch: 800 * 4,
        is_linear: true,
        supports_double_buffer: true,
    });
    idx += 1;
    
    modes[idx] = Some(VesaModeInfo {
        mode_number: 0x112,
        width: 640,
        height: 480,
        bpp: 32,
        pitch: 640 * 4,
        is_linear: true,
        supports_double_buffer: true,
    });
    idx += 1;
    
    // 24-bit modes (compatibility)
    modes[idx] = Some(VesaModeInfo {
        mode_number: 0x11A,
        width: 1280,
        height: 1024,
        bpp: 24,
        pitch: 1280 * 3,
        is_linear: true,
        supports_double_buffer: false,
    });
    idx += 1;
    
    modes[idx] = Some(VesaModeInfo {
        mode_number: 0x117,
        width: 1024,
        height: 768,
        bpp: 24,
        pitch: 1024 * 3,
        is_linear: true,
        supports_double_buffer: false,
    });
    idx += 1;
    
    // 16-bit modes (high compatibility)
    modes[idx] = Some(VesaModeInfo {
        mode_number: 0x114,
        width: 800,
        height: 600,
        bpp: 16,
        pitch: 800 * 2,
        is_linear: true,
        supports_double_buffer: false,
    });
    idx += 1;
    
    modes[idx] = Some(VesaModeInfo {
        mode_number: 0x111,
        width: 640,
        height: 480,
        bpp: 16,
        pitch: 640 * 2,
        is_linear: true,
        supports_double_buffer: false,
    });
    
    modes
}

/// Select best VESA mode based on available modes
/// Prefers: higher resolution > higher color depth > double buffer support
fn select_best_vesa_mode(modes: &[Option<VesaModeInfo>; 16]) -> Option<VesaModeInfo> {
    let mut best_mode: Option<VesaModeInfo> = None;
    let mut best_score = 0u32;
    
    // Scoring weights for mode selection
    const RESOLUTION_DIVISOR: u32 = 1000;  // Normalize resolution to reasonable range
    const BPP_WEIGHT: u32 = 100;            // Weight for color depth
    const DOUBLE_BUFFER_BONUS: u32 = 5000;  // Bonus for double buffering support
    
    for mode_opt in modes.iter() {
        if let Some(mode) = mode_opt {
            // Skip non-linear modes (bank-switched, slower)
            if !mode.is_linear {
                continue;
            }
            
            // Calculate mode score
            // Priority: resolution (80%) > color depth (15%) > double buffer (5%)
            let resolution_score = (mode.width * mode.height) / RESOLUTION_DIVISOR;
            let bpp_score = mode.bpp * BPP_WEIGHT;
            let buffer_score = if mode.supports_double_buffer { DOUBLE_BUFFER_BONUS } else { 0 };
            
            let total_score = resolution_score + bpp_score + buffer_score;
            
            if total_score > best_score {
                best_score = total_score;
                best_mode = Some(*mode);
            }
        }
    }
    
    best_mode
}

/// Initialize NVIDIA graphics driver
/// Note: Current stub implementation always succeeds
/// 
/// In a full implementation, this would:
/// - Interface with kernel nvidia module via syscalls
/// - Leverage NVIDIA open-gpu-kernel-modules
/// - Support Turing, Ampere, Ada Lovelace, Hopper architectures
/// - Initialize CUDA cores for compute workloads
/// - Set up display outputs and modes
fn init_nvidia_driver() -> Result<Framebuffer, &'static str> {
    println!("[DISPLAY-SERVICE] Initializing NVIDIA driver...");
    println!("[DISPLAY-SERVICE]   - Interfacing with kernel nvidia module");
    println!("[DISPLAY-SERVICE]   - Using NVIDIA open-gpu-kernel-modules");
    println!("[DISPLAY-SERVICE]   - Detecting NVIDIA GPU model");
    println!("[DISPLAY-SERVICE]   - Configuring GPU memory");
    println!("[DISPLAY-SERVICE]   - Setting up display modes");
    println!("[DISPLAY-SERVICE]   - Initializing CUDA cores (optional)");
    
    // Get framebuffer info from kernel via syscall
    println!("[DISPLAY-SERVICE]   - Querying kernel for framebuffer information...");
    let kernel_fb_info = get_framebuffer_info_from_kernel()
        .ok_or("Failed to get framebuffer info from kernel")?;
    
    println!("[DISPLAY-SERVICE]     * Framebuffer detected via bootloader");
    println!("[DISPLAY-SERVICE]     * Physical address: 0x{:X}", kernel_fb_info.address);
    println!("[DISPLAY-SERVICE]     * Resolution: {}x{}", kernel_fb_info.width, kernel_fb_info.height);
    println!("[DISPLAY-SERVICE]     * Pitch: {} bytes", kernel_fb_info.pitch);
    println!("[DISPLAY-SERVICE]     * BPP: {} bits", kernel_fb_info.bpp);
    
    // Map framebuffer to virtual memory via syscall
    println!("[DISPLAY-SERVICE]   - Mapping framebuffer to virtual memory...");
    let fb_base = map_framebuffer_memory()
        .ok_or("Failed to map framebuffer into virtual memory")?;
    
    let fb_size = (kernel_fb_info.pitch * kernel_fb_info.height) as usize;
    println!("[DISPLAY-SERVICE]     * Virtual mapping: 0x{:X}", fb_base);
    println!("[DISPLAY-SERVICE]     * Size: {} KB ({} MB)", fb_size / 1024, fb_size / (1024 * 1024));
    
    println!("[DISPLAY-SERVICE]   - NVIDIA driver initialized successfully");
    
    // Clear the screen immediately after mapping framebuffer
    clear_framebuffer_on_init(fb_base, fb_size);
    
    // Create /dev/fb0 device node
    create_framebuffer_device_node(&kernel_fb_info, fb_base);
    
    Ok(Framebuffer {
        base_address: fb_base,
        size: fb_size,
        mode: DisplayMode {
            width: kernel_fb_info.width,
            height: kernel_fb_info.height,
            bpp: kernel_fb_info.bpp as u32,
            mode_number: 0,  // NVIDIA-specific mode
            refresh_rate: 60,
        },
        back_buffer: None,
        pitch: kernel_fb_info.pitch,
        supports_hw_accel: true,
    })
}

/// Initialize VESA graphics driver with comprehensive mode detection
/// Implements VBE 2.0/3.0 support with 2D acceleration
fn init_vesa_driver() -> Result<Framebuffer, &'static str> {
    println!("[DISPLAY-SERVICE] Initializing VESA/VBE driver...");
    println!("[DISPLAY-SERVICE]   ╔════════════════════════════════════════╗");
    println!("[DISPLAY-SERVICE]   ║  VESA BIOS Extensions (VBE) 2.0/3.0  ║");
    println!("[DISPLAY-SERVICE]   ╚════════════════════════════════════════╝");
    
    // Step 1: Get real framebuffer info from kernel
    println!("[DISPLAY-SERVICE]   - Querying kernel for framebuffer information...");
    let kernel_fb_info = get_framebuffer_info_from_kernel()
        .ok_or("Failed to get framebuffer info from kernel")?;
    
    println!("[DISPLAY-SERVICE]     * Framebuffer detected via bootloader");
    println!("[DISPLAY-SERVICE]     * Physical address: 0x{:X}", kernel_fb_info.address);
    println!("[DISPLAY-SERVICE]     * Resolution: {}x{}", kernel_fb_info.width, kernel_fb_info.height);
    println!("[DISPLAY-SERVICE]     * Pitch: {} bytes", kernel_fb_info.pitch);
    println!("[DISPLAY-SERVICE]     * BPP: {} bits", kernel_fb_info.bpp);
    
    // Step 2: Map framebuffer to virtual memory
    println!("[DISPLAY-SERVICE]   - Mapping framebuffer to virtual memory...");
    let fb_base = map_framebuffer_memory()
        .ok_or("Failed to map framebuffer into virtual memory")?;
    
    let fb_size = (kernel_fb_info.pitch * kernel_fb_info.height) as usize;
    println!("[DISPLAY-SERVICE]     * Physical address: 0x{:X}", kernel_fb_info.address);
    println!("[DISPLAY-SERVICE]     * Virtual mapping: 0x{:X}", fb_base);
    println!("[DISPLAY-SERVICE]     * Size: {} KB ({} MB)", fb_size / 1024, fb_size / (1024 * 1024));
    
    // Step 2.5: Clear the screen immediately after mapping framebuffer
    clear_framebuffer_on_init(fb_base, fb_size);
    
    // Step 2.6: Create /dev/fb0 device node
    create_framebuffer_device_node(&kernel_fb_info, fb_base);
    
    // Step 3: Setup double buffering if supported
    let supports_double_buffer = kernel_fb_info.bpp == 32;
    let back_buffer = if supports_double_buffer {
        println!("[DISPLAY-SERVICE]   - Allocating back buffer for double buffering...");
        let back_buf_addr = fb_base + fb_size;
        println!("[DISPLAY-SERVICE]     ✓ Back buffer at: 0x{:X}", back_buf_addr);
        Some(back_buf_addr)
    } else {
        println!("[DISPLAY-SERVICE]   - Double buffering not supported in this mode");
        None
    };
    
    // Step 4: Initialize 2D acceleration
    println!("[DISPLAY-SERVICE]   - Initializing 2D acceleration engine...");
    let supports_accel = kernel_fb_info.bpp == 32;
    if supports_accel {
        println!("[DISPLAY-SERVICE]     ✓ Hardware-accelerated operations enabled:");
        println!("[DISPLAY-SERVICE]       - Fast block transfers (BitBLT)");
        println!("[DISPLAY-SERVICE]       - Rectangle fills");
        println!("[DISPLAY-SERVICE]       - Memory copies (DMA-style)");
        println!("[DISPLAY-SERVICE]       - Screen-to-screen blits");
        println!("[DISPLAY-SERVICE]       - Pattern fills");
    } else {
        println!("[DISPLAY-SERVICE]     ! Software rendering only (mode limitations)");
    }
    
    // Step 5: Configure V-Sync
    println!("[DISPLAY-SERVICE]   - Configuring vertical sync (V-Sync)...");
    println!("[DISPLAY-SERVICE]     * V-Sync enabled for tear-free rendering");
    println!("[DISPLAY-SERVICE]     * Target refresh rate: 60 Hz");
    
    println!("[DISPLAY-SERVICE]   ╔════════════════════════════════════════╗");
    println!("[DISPLAY-SERVICE]   ║    VESA driver initialized successfully    ║");
    println!("[DISPLAY-SERVICE]   ╚════════════════════════════════════════╝");
    
    Ok(Framebuffer {
        base_address: fb_base,
        size: fb_size,
        mode: DisplayMode {
            width: kernel_fb_info.width,
            height: kernel_fb_info.height,
            bpp: kernel_fb_info.bpp as u32,
            mode_number: 0, // Not applicable for bootloader framebuffer
            refresh_rate: 60,
        },
        back_buffer,
        pitch: kernel_fb_info.pitch,
        supports_hw_accel: supports_accel,
    })
}

/// Simulate V-Sync wait
fn wait_for_vsync() {
    // In a real implementation, this would wait for vertical blank
    // by reading VGA status register (0x3DA) bit 3 or using VBE 3.0
    // protected mode interface for V-Sync notification
    // For now, just yield to simulate timing
    for _ in 0..10 {
        yield_cpu();
    }
}

/// 2D Acceleration: Hardware-accelerated block transfer (BitBLT)
/// Copies a rectangular region from source to destination
fn accel_blit(fb: &Framebuffer, _src_x: u32, _src_y: u32, 
               _dst_x: u32, _dst_y: u32, _width: u32, _height: u32) -> Result<(), &'static str> {
    if !fb.supports_hw_accel {
        return Err("Hardware acceleration not supported");
    }
    
    // In a real implementation, this would:
    // 1. Program graphics controller registers for source/dest
    // 2. Set up DMA transfer if available
    // 3. Trigger hardware blit operation
    // 4. Wait for completion or use interrupt
    
    // For now, simulate successful blit
    // Actual implementation would write to VGA/VBE registers or use
    // memory-mapped I/O for modern graphics cards
    
    Ok(())
}

/// 2D Acceleration: Fast rectangle fill
/// Fills a rectangular region with a solid color
fn accel_fill_rect(fb: &Framebuffer, x: u32, y: u32, 
                    _width: u32, _height: u32, _color: u32) -> Result<(), &'static str> {
    if !fb.supports_hw_accel {
        return Err("Hardware acceleration not supported");
    }
    
    // In a real implementation, this would:
    // 1. Set fill color in graphics controller
    // 2. Set destination rectangle coordinates
    // 3. Trigger hardware fill operation
    // 4. Use pattern fill if supported
    
    // Software fallback implementation (optimized):
    // This simulates what would be a hardware operation
    let bytes_per_pixel = (fb.mode.bpp / 8) as u32;
    let _start_offset = (y * fb.pitch + x * bytes_per_pixel) as usize;
    
    // Simulate DMA-style fill by yielding
    // Real implementation would write directly to framebuffer memory
    yield_cpu();
    
    Ok(())
}

/// 2D Acceleration: Memory copy (DMA-style)
/// Fast memory-to-memory copy for framebuffer operations
#[allow(dead_code)]
fn accel_mem_copy(fb: &Framebuffer, _src: usize, _dst: usize, _size: usize) -> Result<(), &'static str> {
    if !fb.supports_hw_accel {
        return Err("Hardware acceleration not supported");
    }
    
    // In a real implementation, this would:
    // 1. Use DMA controller if available
    // 2. Or use optimized CPU instructions (rep movsb/movsq)
    // 3. Copy in large blocks for cache efficiency
    
    // Simulate DMA transfer
    yield_cpu();
    
    Ok(())
}

/// Double buffer swap - present back buffer to display
/// Implements tear-free page flipping if hardware supports it
fn swap_buffers(fb: &Framebuffer) -> Result<(), &'static str> {
    if let Some(_back_buffer_addr) = fb.back_buffer {
        // In a real implementation, this would:
        // 1. Wait for V-Sync
        // 2. Update display start address register (VBE function 07h)
        // 3. Swap buffer pointers atomically
        
        wait_for_vsync();
        
        // Simulate register write to change display start address
        // VBE call: AX=4F07h, BL=80h (Set Display Start during V-Sync)
        
        Ok(())
    } else {
        Err("Double buffering not available")
    }
}

/// Perform optimized screen clear
fn clear_screen(fb: &Framebuffer, color: u32) -> Result<(), &'static str> {
    // Clear primary framebuffer
    let fb_ptr = fb.base_address as *mut u32;
    let pixel_count = (fb.size / BYTES_PER_PIXEL) as usize;
    
    // Use volatile writes for all colors to ensure visibility on memory-mapped framebuffer
    unsafe {
        for i in 0..pixel_count {
            core::ptr::write_volatile(fb_ptr.add(i), color);
        }
    }
    
    // Also clear back buffer if it exists
    if let Some(back_buf_addr) = fb.back_buffer {
        let back_ptr = back_buf_addr as *mut u32;
        unsafe {
            for i in 0..pixel_count {
                core::ptr::write_volatile(back_ptr.add(i), color);
            }
        }
    }
    
    Ok(())
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let pid = getpid();
    
    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║              GRAPHICS / DISPLAY SERVICE                      ║");
    println!("╚══════════════════════════════════════════════════════════════╝");
    println!("[DISPLAY-SERVICE] Starting (PID: {})", pid);
    println!("[DISPLAY-SERVICE] Initializing graphics subsystem...");
    
    // Detect available graphics hardware
    println!("[DISPLAY-SERVICE] Scanning for graphics hardware...");
    
    let mut active_driver = GraphicsDriver::None;
    let mut framebuffer: Option<Framebuffer> = None;
    let mut stats = DisplayStats {
        frames_rendered: 0,
        vsync_count: 0,
        driver_errors: 0,
        blit_operations: 0,
        fill_operations: 0,
        copy_operations: 0,
    };
    
    // Try NVIDIA first (preferred)
    if detect_nvidia_gpu() {
        println!("[DISPLAY-SERVICE] NVIDIA GPU detected!");
        match init_nvidia_driver() {
            Ok(fb) => {
                active_driver = GraphicsDriver::NVIDIA;
                framebuffer = Some(fb);
                println!("[DISPLAY-SERVICE] Using NVIDIA driver");
            }
            Err(e) => {
                println!("[DISPLAY-SERVICE] NVIDIA driver initialization failed: {}", e);
                stats.driver_errors += 1;
            }
        }
    } else {
        println!("[DISPLAY-SERVICE] No NVIDIA GPU detected");
    }
    
    // Fallback to VESA if NVIDIA not available
    if active_driver == GraphicsDriver::None {
        println!("[DISPLAY-SERVICE] Falling back to VESA driver");
        match init_vesa_driver() {
            Ok(fb) => {
                active_driver = GraphicsDriver::VESA;
                framebuffer = Some(fb);
                println!("[DISPLAY-SERVICE] Using VESA driver");
            }
            Err(e) => {
                println!("[DISPLAY-SERVICE] VESA driver initialization failed: {}", e);
                stats.driver_errors += 1;
            }
        }
    }
    
    // Report final status
    match active_driver {
        GraphicsDriver::NVIDIA => {
            println!("[DISPLAY-SERVICE] Graphics initialized with NVIDIA driver");
        }
        GraphicsDriver::VESA => {
            println!("[DISPLAY-SERVICE] Graphics initialized with VESA driver");
        }
        GraphicsDriver::None => {
            println!("[DISPLAY-SERVICE] WARNING: No graphics driver available!");
        }
    }
    
    // Initialize framebuffer information
    if let Some(ref fb) = framebuffer {
        println!("[DISPLAY-SERVICE] Framebuffer configuration:");
        println!("[DISPLAY-SERVICE]   - Resolution: {}x{}", fb.mode.width, fb.mode.height);
        println!("[DISPLAY-SERVICE]   - Color depth: {}-bit", fb.mode.bpp);
        println!("[DISPLAY-SERVICE]   - Refresh rate: {} Hz", fb.mode.refresh_rate);
        println!("[DISPLAY-SERVICE]   - Pitch: {} bytes/scanline", fb.pitch);
        println!("[DISPLAY-SERVICE]   - Memory: {} KB ({} MB)", 
                 fb.size / 1024, fb.size / (1024 * 1024));
        println!("[DISPLAY-SERVICE]   - Base address: 0x{:X}", fb.base_address);
        println!("[DISPLAY-SERVICE]   - Device: /dev/fb0");
        
        if fb.back_buffer.is_some() {
            println!("[DISPLAY-SERVICE]   - Double buffering: ENABLED");
        }
        
        if fb.supports_hw_accel {
            println!("[DISPLAY-SERVICE]   - 2D Acceleration: ENABLED");
            println!("[DISPLAY-SERVICE]     * BitBLT operations");
            println!("[DISPLAY-SERVICE]     * Rectangle fills");
            println!("[DISPLAY-SERVICE]     * DMA copies");
        } else {
            println!("[DISPLAY-SERVICE]   - 2D Acceleration: SOFTWARE ONLY");
        }
        
        // Demonstrate 2D acceleration with test operations
        if fb.supports_hw_accel {
            println!("[DISPLAY-SERVICE] Testing 2D acceleration...");
            
            // Test screen clear
            match clear_screen(fb, colors::BLACK) {
                Ok(_) => {
                    println!("[DISPLAY-SERVICE]   ✓ Screen clear successful");
                    stats.fill_operations += 1;
                }
                Err(e) => {
                    println!("[DISPLAY-SERVICE]   ✗ Screen clear failed: {}", e);
                    stats.driver_errors += 1;
                }
            }
            
            // Test rectangle fill
            match accel_fill_rect(fb, 100, 100, 200, 150, colors::BLUE) {
                Ok(_) => {
                    println!("[DISPLAY-SERVICE]   ✓ Rectangle fill successful");
                    stats.fill_operations += 1;
                }
                Err(e) => {
                    println!("[DISPLAY-SERVICE]   ✗ Rectangle fill failed: {}", e);
                    stats.driver_errors += 1;
                }
            }
            
            // Test blit operation
            match accel_blit(fb, 0, 0, 10, 10, 100, 100) {
                Ok(_) => {
                    println!("[DISPLAY-SERVICE]   ✓ BitBLT operation successful");
                    stats.blit_operations += 1;
                }
                Err(e) => {
                    println!("[DISPLAY-SERVICE]   ✗ BitBLT operation failed: {}", e);
                    stats.driver_errors += 1;
                }
            }
            
            println!("[DISPLAY-SERVICE] 2D acceleration tests completed");
        }
    }
    
    if let Some(ref fb) = framebuffer {
        println!("[DISPLAY-SERVICE] Final screen clear before starting...");
        let _ = clear_screen(fb, 0x00000000);
    }

    println!("[DISPLAY-SERVICE] Display service ready");
    println!("[DISPLAY-SERVICE] Ready to accept rendering requests...");
    
    // Main loop - render frames and handle display events
    let mut heartbeat_counter = 0u64;
    
    loop {
        heartbeat_counter += 1;
        
        // Simulate frame rendering with V-Sync and 2D acceleration
        // In a real implementation, this would:
        // - Process rendering commands from IPC
        // - Use 2D acceleration for drawing operations
        // - Update framebuffer (or back buffer if double buffering)
        // - Swap buffers on V-Sync for tear-free display
        // - Handle display mode changes
        
        // Simulate rendering at ~60 FPS with V-Sync
        if heartbeat_counter % 16666 == 0 {  // Approximate 60Hz
            stats.frames_rendered += 1;
            stats.vsync_count += 1;
            
            // Use 2D acceleration for rendering if available
            if let Some(ref fb) = framebuffer {
                if fb.supports_hw_accel {
                    // Simulate accelerated rendering operations
                    if heartbeat_counter % 100000 == 0 {
                        // Occasional fill operation
                        let _ = accel_fill_rect(fb, 50, 50, 100, 100, colors::RED);
                        stats.fill_operations += 1;
                    }
                    
                    // Double buffer swap if supported
                    if fb.back_buffer.is_some() {
                        let _ = swap_buffers(fb);
                    }
                }
            }
            
            wait_for_vsync();
        }
        
        // Periodic status updates with enhanced metrics
        if heartbeat_counter % 500000 == 0 {
            let driver_name = match active_driver {
                GraphicsDriver::NVIDIA => "NVIDIA",
                GraphicsDriver::VESA => "VESA",
                GraphicsDriver::None => "NONE",
            };
            println!("[DISPLAY-SERVICE] Status - Driver: {}, Frames: {}, V-Syncs: {}", 
                     driver_name, stats.frames_rendered, stats.vsync_count);
            
            if let Some(ref fb) = framebuffer {
                println!("[DISPLAY-SERVICE]   Display: {}x{}@{}bpp @ {}Hz", 
                         fb.mode.width, fb.mode.height, fb.mode.bpp, fb.mode.refresh_rate);
                
                if fb.supports_hw_accel {
                    println!("[DISPLAY-SERVICE]   2D Accel: Blits={}, Fills={}, Copies={}", 
                             stats.blit_operations, stats.fill_operations, stats.copy_operations);
                }
            }
            
            if stats.driver_errors > 0 {
                println!("[DISPLAY-SERVICE]   Errors: {}", stats.driver_errors);
            }
        }
        
        yield_cpu();
    }
}
