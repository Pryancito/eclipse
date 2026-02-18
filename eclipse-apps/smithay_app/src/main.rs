//! Smithay App - Xwayland Compositor
//! 
//! This application implements a Wayland compositor with Xwayland support
//! using Eclipse OS IPC and /dev/fb0 framebuffer device.

#![no_std]
#![no_main]



use eclipse_libc::{println, getpid, send, receive, yield_cpu, get_framebuffer_info, map_framebuffer, FramebufferInfo};
use embedded_graphics::{
    pixelcolor::Rgb888,
    prelude::*,
    primitives::{Rectangle, PrimitiveStyleBuilder, CornerRadii},
    text::{Text, TextStyle},
    mono_font::{ascii::FONT_10X20, MonoTextStyle},
};

/// IPC Message Types
const MSG_TYPE_GRAPHICS: u32 = 0x00000010;  // Graphics messages

#[allow(dead_code)]
const MSG_TYPE_INPUT: u32 = 0x00000040;     // Input messages

#[allow(dead_code)]
const MSG_TYPE_SIGNAL: u32 = 0x00000400;    // Signal messages

/// Status update interval (iterations between status prints)
const STATUS_UPDATE_INTERVAL: u64 = 1000000;

/// IPC message buffer size
const IPC_BUFFER_SIZE: usize = 256;

/// Input event layout shared with `userspace/input_service`
#[repr(C)]
#[derive(Clone, Copy)]
struct InputEvent {
    device_id: u32,
    event_type: u8,  // 0=key, 1=mouse_move, 2=mouse_button
    code: u16,
    value: i32,
    timestamp: u64,
}

/// Simple input state (logical cursor)
struct InputState {
    cursor_x: i32,
    cursor_y: i32,
}

impl InputState {
    fn new(width: i32, height: i32) -> Self {
        Self {
            cursor_x: width / 2,
            cursor_y: height / 2,
        }
    }

    fn apply_event(&mut self, ev: &InputEvent, fb_width: i32, fb_height: i32) {
        match ev.event_type {
            // Keyboard: for now just log
            0 => {
                println!(
                    "[SMITHAY] Key event: code=0x{:x} value={} device_id={}",
                    ev.code, ev.value, ev.device_id
                );
            }
            // Mouse move (input_service simulates X axis with code=0)
            1 => {
                if ev.code == 0 {
                    self.cursor_x = (self.cursor_x + ev.value)
                        .clamp(0, fb_width.saturating_sub(1));
                }
            }
            _ => {}
        }

        // Clamp Y in case we add vertical movement later
        self.cursor_y = self
            .cursor_y
            .clamp(0, fb_height.saturating_sub(1));
    }
}

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
    
    /// Process one IPC message (if any) and return a possible `InputEvent`.
    fn process_messages(&mut self) -> Option<InputEvent> {
        let mut buffer = [0u8; IPC_BUFFER_SIZE];
        
        let (len, sender_pid) = receive(&mut buffer);
        
        if len > 0 {
            self.message_count += 1;
            println!("[SMITHAY] Received IPC message from PID {}: {} bytes", 
                sender_pid, len);

            // Check if this is a binary input event from input_service
            if len == core::mem::size_of::<InputEvent>() {
                let mut ev = InputEvent {
                    device_id: 0,
                    event_type: 0,
                    code: 0,
                    value: 0,
                    timestamp: 0,
                };
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        buffer.as_ptr(),
                        &mut ev as *mut InputEvent as *mut u8,
                        core::mem::size_of::<InputEvent>(),
                    );
                }
                return Some(ev);
            }

            let response = b"ACK";
            if send(sender_pid, MSG_TYPE_GRAPHICS, response) != 0 {
                println!("[SMITHAY] WARNING: Failed to send ACK to PID {}", sender_pid);
            }
        }

        None
    }
}

/// Ask init (PID 1) for the PID of `input_service`.
fn query_input_service_pid() -> Option<u32> {
    const INIT_PID: u32 = 1;
    const REQUEST: &[u8] = b"GET_INPUT_PID";

    if send(INIT_PID, MSG_TYPE_INPUT, REQUEST) != 0 {
        println!("[SMITHAY] ERROR: Failed to send GET_INPUT_PID to init");
        return None;
    }

    let mut buffer = [0u8; IPC_BUFFER_SIZE];

    // Small non-blocking wait loop
    for _ in 0..1000 {
        let (len, sender_pid) = receive(&mut buffer);
        if len >= 8 && sender_pid == INIT_PID && &buffer[0..4] == b"INPT" {
            let mut id_bytes = [0u8; 4];
            id_bytes.copy_from_slice(&buffer[4..8]);
            let pid = u32::from_le_bytes(id_bytes);
            if pid != 0 {
                println!("[SMITHAY] input_service PID discovered: {}", pid);
                return Some(pid);
            }
        }
        yield_cpu();
    }

    println!("[SMITHAY] WARNING: Could not get input_service PID from init");
    None
}

/// Send a subscription message to `input_service` with our own PID.
fn subscribe_to_input_service(input_pid: u32, self_pid: u32) {
    let mut msg = [0u8; 8];
    msg[0..4].copy_from_slice(b"SUBS");
    msg[4..8].copy_from_slice(&self_pid.to_le_bytes());
    let res = send(input_pid, MSG_TYPE_INPUT, &msg);
    if res != 0 {
        println!(
            "[SMITHAY] WARNING: Failed to send SUBS to input_service (PID {}), code={}",
            input_pid, res
        );
    } else {
        println!(
            "[SMITHAY] Subscribed to input_service events (PID {})",
            input_pid
        );
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

    // Simple input state (logical cursor centered on screen)
    let mut input_state = InputState::new(
        fb.info.width as i32,
        fb.info.height as i32,
    );
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

    // Discover input_service PID and subscribe to its events
    if let Some(input_pid) = query_input_service_pid() {
        subscribe_to_input_service(input_pid, pid);
    }

    // Initialize IPC handler
    let mut ipc = IpcHandler::new();
    
    println!("[SMITHAY] Compositor ready and running");

    let mut counter: u64 = 0;
    let mut last_status_counter: u64 = 0;
    
    loop {
        counter = counter.wrapping_add(1);
        // Process IPC messages and update input state if we receive events
        if let Some(ev) = ipc.process_messages() {
            input_state.apply_event(
                &ev,
                fb.info.width as i32,
                fb.info.height as i32,
            );
        }
        
        if counter.wrapping_sub(last_status_counter) >= STATUS_UPDATE_INTERVAL {
            // Update the status bar text periodically
            Rectangle::new(Point::new(0, fb.info.height as i32 - 40), Size::new(fb.info.width as u32, 40))
                .into_styled(PrimitiveStyleBuilder::new().fill_color(Rgb888::new(0, 80, 150)).build())
                .draw(&mut fb).unwrap();

            Text::new("Status: Running | Clients: 0 | IPC/Input active", Point::new(20, fb.info.height as i32 - 15), 
                      MonoTextStyle::new(&FONT_10X20, Rgb888::WHITE))
                .draw(&mut fb).unwrap();
            
            last_status_counter = counter;
        }
        
        yield_cpu();
    }
}

// Panic handler is provided by eclipse-libc
