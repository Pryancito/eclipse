//! Smithay App - Xwayland Compositor
//! 
//! This application implements a Wayland compositor with Xwayland support
//! using Eclipse OS IPC and /dev/fb0 framebuffer device.

#![no_std]
#![no_main]



use eclipse_libc::{println, getpid, yield_cpu, get_framebuffer_info, map_framebuffer, send, receive, FramebufferInfo};
use embedded_graphics::{
    pixelcolor::Rgb888,
    prelude::*,
    primitives::{Rectangle, PrimitiveStyleBuilder, CornerRadii},
    text::{Text, TextStyle},
    mono_font::{ascii::FONT_10X20, MonoTextStyle},
};

/// IPC Message Types for Xwayland protocol
const MSG_TYPE_GRAPHICS: u32 = 0x00000010;  // Graphics messages

#[allow(dead_code)]
const MSG_TYPE_INPUT: u32 = 0x00000040;     // Input messages

#[allow(dead_code)]
const MSG_TYPE_SIGNAL: u32 = 0x00000400;    // Signal messages

/// Status update interval (iterations between status prints)
const STATUS_UPDATE_INTERVAL: u64 = 1000000;

/// IPC message buffer size
const IPC_BUFFER_SIZE: usize = 256;

/// Framebuffer state
struct FramebufferState {
    info: FramebufferInfo,
    base_addr: usize,
}

impl FramebufferState {
    /// Initialize framebuffer by getting info and mapping it
    fn init() -> Option<Self> {
        println!("[SMITHAY] Initializing framebuffer access...");
        
        let fb_info = match get_framebuffer_info() {
            Some(info) => {
                println!("[SMITHAY]   - Framebuffer: {}x{} @ {} bpp", 
                    info.width, info.height, info.bpp);
                info
            }
            None => {
                println!("[SMITHAY]   - ERROR: Failed to get framebuffer info");
                return None;
            }
        };
        
        let fb_base = match map_framebuffer() {
            Some(addr) => {
                println!("[SMITHAY]   - Framebuffer mapped at address: 0x{:x}", addr);
                addr
            }
            None => {
                println!("[SMITHAY]   - ERROR: Failed to map framebuffer");
                return None;
            }
        };
        
        Some(FramebufferState {
            info: fb_info,
            base_addr: fb_base,
        })
    }
}

/// DrawTarget implementation for our Framebuffer
impl DrawTarget for FramebufferState {
    type Color = Rgb888;
    type Error = core::convert::Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        let width = self.info.width as i32;
        let height = self.info.height as i32;
        let fb_ptr = self.base_addr as *mut u32;

        for Pixel(coord, color) in pixels.into_iter() {
            if coord.x >= 0 && coord.x < width && coord.y >= 0 && coord.y < height {
                let offset = (coord.y * width + coord.x) as usize;
                // Convert Rgb888 to ARGB8888 (0xFFRRGGBB)
                let raw_color = 0xFF000000 | 
                    ((color.r() as u32) << 16) | 
                    ((color.g() as u32) << 8) | 
                    (color.b() as u32);
                
                unsafe {
                    core::ptr::write_volatile(fb_ptr.add(offset), raw_color);
                }
            }
        }
        Ok(())
    }
}

impl OriginDimensions for FramebufferState {
    fn size(&self) -> Size {
        Size::new(self.info.width as u32, self.info.height as u32)
    }
}

/// IPC Communication Handler
struct IpcHandler {
    message_count: u64,
}

impl IpcHandler {
    fn new() -> Self {
        Self {
            message_count: 0,
        }
    }
    
    /// Process incoming IPC messages
    fn process_messages(&mut self) {
        let mut buffer = [0u8; IPC_BUFFER_SIZE];
        
        let (len, sender_pid) = receive(&mut buffer);
        
        if len > 0 {
            self.message_count += 1;
            println!("[SMITHAY] Received IPC message from PID {}: {} bytes", 
                sender_pid, len);
            
            let response = b"ACK";
            if send(sender_pid, MSG_TYPE_GRAPHICS, response) != 0 {
                println!("[SMITHAY] WARNING: Failed to send ACK to PID {}", sender_pid);
            }
        }
    }
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    // Re-align stack to 16 bytes for SSE instructions.
    // Some linkers/stubs might have CALLED us, misaligning RSP by 8.
    unsafe {
        core::arch::asm!("and rsp, -16", options(nomem, nostack));
    }
    
    let pid = getpid();
    
    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║         SMITHAY XWAYLAND COMPOSITOR v0.2.1                   ║");
    println!("║         (Rust Native Display Server Prototype)               ║");
    println!("╚══════════════════════════════════════════════════════════════╝");
    
    // Initialize framebuffer
    let mut fb = match FramebufferState::init() {
        Some(fb) => fb,
        None => {
            println!("[SMITHAY] CRITICAL: Cannot start without display");
            loop { yield_cpu(); }
        }
    };
    
    // Clear screen
    fb.clear(Rgb888::new(26, 26, 26)).unwrap();
    
    // Draw Header
    let rect_style = PrimitiveStyleBuilder::new()
        .fill_color(Rgb888::new(45, 45, 45))
        .stroke_color(Rgb888::new(100, 100, 255))
        .stroke_width(2)
        .build();
    
    Rectangle::new(Point::new(50, 50), Size::new(700, 100))
        .into_styled(rect_style)
        .draw(&mut fb).unwrap();

    let text_style = MonoTextStyle::new(&FONT_10X20, Rgb888::WHITE);
    Text::new("Eclipse OS - Rust Display Server", Point::new(80, 110), text_style)
        .draw(&mut fb).unwrap();

    // Draw Footer/Status Bar
    Rectangle::new(Point::new(0, fb.info.height as i32 - 40), Size::new(fb.info.width as u32, 40))
        .into_styled(PrimitiveStyleBuilder::new().fill_color(Rgb888::new(0, 80, 150)).build())
        .draw(&mut fb).unwrap();

    Text::new("Ready | Wayland: 0 | X11: 0 | PID: 10", Point::new(20, fb.info.height as i32 - 15), 
              MonoTextStyle::new(&FONT_10X20, Rgb888::WHITE))
        .draw(&mut fb).unwrap();

    // Initialize IPC handler
    let mut ipc = IpcHandler::new();
    
    println!("[SMITHAY] Compositor ready and running");

    let mut counter: u64 = 0;
    let mut last_status_counter: u64 = 0;
    
    loop {
        counter = counter.wrapping_add(1);
        ipc.process_messages();
        
        if counter.wrapping_sub(last_status_counter) >= STATUS_UPDATE_INTERVAL {
            // Update the status bar text periodically
            Rectangle::new(Point::new(0, fb.info.height as i32 - 40), Size::new(fb.info.width as u32, 40))
                .into_styled(PrimitiveStyleBuilder::new().fill_color(Rgb888::new(0, 80, 150)).build())
                .draw(&mut fb).unwrap();

            Text::new("Status: Running | Clients: 0 | IPC Msgs received", Point::new(20, fb.info.height as i32 - 15), 
                      MonoTextStyle::new(&FONT_10X20, Rgb888::WHITE))
                .draw(&mut fb).unwrap();
            
            last_status_counter = counter;
        }
        
        yield_cpu();
    }
}

// Panic handler is provided by eclipse-libc
