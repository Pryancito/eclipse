#![no_std]
#![no_main]

use eclipse_libc::{println, yield_cpu};
use sidewind_sdk::{discover_composer, SideWindSurface};
use sidewind_core::{SWND_EVENT_TYPE_KEY, SWND_EVENT_TYPE_MOUSE_MOVE, SWND_EVENT_TYPE_RESIZE};

#[no_mangle]
pub extern "C" fn _start() -> ! {
    println!("[DEMO] Starting SideWind Demo Client...");

    let composer_pid = loop {
        if let Some(pid) = discover_composer() {
            println!("[DEMO] Discovered compositor at PID {}", pid);
            break pid;
        }
        yield_cpu();
    };

    let mut surface = SideWindSurface::new(composer_pid, 200, 200, 400, 300, "demo_buffer")
        .expect("[DEMO] Failed to create surface");

    println!("[DEMO] Surface created (400x300). Rendering animation...");

    let mut frame = 0u32;
    let mut mx = 0i32;
    let mut my = 0i32;

    loop {
        // Poll events
        while let Some(ev) = surface.poll_event() {
            match ev.event_type {
                SWND_EVENT_TYPE_MOUSE_MOVE => {
                    mx = ev.data1;
                    my = ev.data2;
                }
                SWND_EVENT_TYPE_KEY => {
                    println!("[DEMO] Key Event: code={}, value={}", ev.data1, ev.data2);
                }
                SWND_EVENT_TYPE_RESIZE => {
                    println!("[DEMO] Resize Event: {}x{}", ev.data1, ev.data2);
                    surface.set_size(ev.data1 as u32, ev.data2 as u32);
                }
                _ => {}
            }
        }

        let w = surface.width() as usize;
        let h = surface.height() as usize;
        let buffer = surface.buffer();

        // Render a simple animated gradient
        for y in 0..h {
            for x in 0..w {
                let r = ((x as u32 + frame) % 256) as u32;
                let g = ((y as u32 + frame) % 256) as u32;
                let b = (frame % 256) as u32;
                buffer[y * w + x] = 0xFF000000 | (r << 16) | (g << 8) | b;
            }
        }

        // Draw a small white box at mouse position
        for dy in -5..5 {
            for dx in -5..5 {
                let px = mx + dx;
                let py = my + dy;
                if px >= 0 && px < w as i32 && py >= 0 && py < h as i32 {
                    buffer[(py as usize) * w + (px as usize)] = 0xFFFFFFFF;
                }
            }
        }

        surface.commit();
        frame = frame.wrapping_add(2);

        // Throttle
        for _ in 0..10 {
            yield_cpu();
        }
    }
}
