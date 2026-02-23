#![no_std]
#![no_main]

extern crate alloc;
extern crate eclipse_syscall;

use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicUsize, Ordering};
use eclipse_libc::{println, getpid, yield_cpu};
use embedded_graphics::prelude::*;
use embedded_graphics::pixelcolor::Rgb888;
use embedded_graphics::primitives::{Rectangle, PrimitiveStyleBuilder};

mod compositor;
mod render;
mod input;
mod ipc;

use compositor::{ShellWindow, ExternalSurface, WindowContent, MAX_WINDOWS_COUNT, MAX_EXTERNAL_SURFACES, next_visible};
use render::{FramebufferState, draw_static_ui, draw_shell_windows, draw_cursor, draw_dashboard, draw_lock_screen, draw_notifications, draw_search_hud, draw_launcher, draw_quick_settings, draw_context_menu, draw_alt_tab_hud, STROKE_COLORS};
use input::{InputState, CompositorEvent};
use ipc::{IpcHandler, query_input_service_pid, subscribe_to_input_service, handle_sidewind_message};

const HEAP_SIZE: usize = 8 * 1024 * 1024; // 8MB
#[repr(align(4096))]
struct Heap([u8; HEAP_SIZE]);
static mut HEAP: Heap = Heap([0u8; HEAP_SIZE]);
static HEAP_PTR: AtomicUsize = AtomicUsize::new(0);

struct StaticAllocator;
unsafe impl GlobalAlloc for StaticAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let align = layout.align();
        let size = layout.size();
        loop {
            let current = HEAP_PTR.load(Ordering::SeqCst);
            let aligned = (current + align - 1) & !(align - 1);
            if aligned + size > HEAP_SIZE { return core::ptr::null_mut(); }
            if HEAP_PTR.compare_exchange(current, aligned + size, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                return HEAP.0.as_mut_ptr().add(aligned);
            }
        }
    }
    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
}

#[global_allocator]
static ALLOCATOR: StaticAllocator = StaticAllocator;

#[no_mangle]
pub extern "C" fn _start() -> ! {
    unsafe { core::arch::asm!("and rsp, -16", options(nomem, nostack, preserves_flags)); }
    
    println!("[SMITHAY] Restoring Desktop Environment...");
    let pid = getpid();
    
    let mut fb = match FramebufferState::init() {
        Some(fb) => fb,
        None => { println!("[SMITHAY] FATAL: Display init failed"); loop { yield_cpu(); } }
    };
    fb.pre_render_background();

    let mut input_state = InputState::new(fb.info.width as i32, fb.info.height as i32);
    let mut ipc = IpcHandler::new();
    let mut windows = [ShellWindow { x: 0, y: 0, w: 0, h: 0, curr_x: 0.0, curr_y: 0.0, curr_w: 0.0, curr_h: 0.0, minimized: false, maximized: false, closing: false, stored_rect: (0,0,0,0), workspace: 0, content: WindowContent::None }; MAX_WINDOWS_COUNT];
    let mut window_count = 0;
    let mut surfaces = [ExternalSurface { id: 0, pid: 0, vaddr: 0, buffer_size: 0, active: false }; MAX_EXTERNAL_SURFACES];
    
    // Initial demo window
    windows[0] = ShellWindow {
        x: 100, y: 100, w: 400, h: 300,
        curr_x: (100 + 400/2) as f32, curr_y: (100 + 300/2) as f32, curr_w: 0.0, curr_h: 0.0,
        minimized: false, maximized: false, closing: false, stored_rect: (100, 100, 400, 300),
        workspace: 0, content: WindowContent::InternalDemo,
    };
    window_count = 1;

    if let Some(in_pid) = query_input_service_pid() { subscribe_to_input_service(in_pid, pid); }

    let mut counter: u64 = 0;
    loop {
        counter += 1;
        
        while let Some(event) = ipc.process_messages() {
            match event {
                CompositorEvent::Input(ev) => input_state.apply_event(&ev, fb.info.width as i32, fb.info.height as i32, &mut windows, &mut window_count, &surfaces),
                CompositorEvent::SideWind(msg, sender) => handle_sidewind_message(msg, sender, &mut surfaces, &mut windows, &mut window_count, &mut input_state),
                _ => {}
            }
        }

        // --- Handle Window Lifecycle Requests ---
        if input_state.request_close_window && window_count > 0 {
            let idx = input_state.focused_window.unwrap_or(window_count - 1);
            windows[idx].closing = true;
            input_state.request_close_window = false;
            input_state.dragging_window = None;
            input_state.focused_window = None;
        }
        if input_state.request_new_window && window_count < MAX_WINDOWS_COUNT {
            windows[window_count] = ShellWindow {
                x: 60 + (window_count as i32) * 20, y: 160 + (window_count as i32) * 15, w: 240, h: 180,
                curr_x: (60 + (window_count as i32) * 20 + 120) as f32, curr_y: (160 + (window_count as i32) * 15 + 90) as f32,
                curr_w: 0.0, curr_h: 0.0, minimized: false, maximized: false, closing: false,
                stored_rect: (60 + (window_count as i32) * 20, 160 + (window_count as i32) * 15, 240, 180),
                workspace: input_state.current_workspace, content: WindowContent::InternalDemo,
            };
            window_count += 1;
            input_state.request_new_window = false;
        }
        if input_state.request_cycle_forward && window_count > 1 {
            let current = input_state.focused_window.unwrap_or(0);
            if let Some(next) = next_visible(current, true, &windows, window_count) {
                let top = window_count - 1;
                windows.swap(next, top);
                input_state.focused_window = Some(top);
            }
            input_state.request_cycle_forward = false;
        }
        if input_state.request_dashboard {
            input_state.dashboard_active = !input_state.dashboard_active;
            input_state.request_dashboard = false;
        }

        // --- Animations (LERP) ---
        let mut min_count_anim = 0;
        let mut i = 0;
        while i < window_count {
            let (tx, ty, tw, th) = if windows[i].closing {
                (windows[i].curr_x + windows[i].curr_w / 2.0, windows[i].curr_y + windows[i].curr_h / 2.0, 0.0, 0.0)
            } else if windows[i].minimized {
                let px = (100 + (min_count_anim % 3) * 120) as f32;
                let py = (250 + (min_count_anim / 3) * 150) as f32;
                min_count_anim += 1;
                (px - 20.0, py - 40.0, 40.0, 40.0)
            } else {
                (windows[i].x as f32, windows[i].y as f32, windows[i].w as f32, windows[i].h as f32)
            };

            let lerp = if windows[i].closing { 0.32 } else { 0.22 };
            windows[i].curr_x += (tx - windows[i].curr_x) * lerp;
            windows[i].curr_y += (ty - windows[i].curr_y) * lerp;
            windows[i].curr_w += (tw - windows[i].curr_w) * lerp;
            windows[i].curr_h += (th - windows[i].curr_h) * lerp;

            if windows[i].closing && windows[i].curr_w < 5.0 {
                for j in i..(window_count - 1) { windows[j] = windows[j+1]; }
                window_count -= 1;
            } else { i += 1; }
        }

        // Overlay Interpolations
        let target_notif_x = if input_state.notifications_active { (fb.info.width as i32 - 300) as f32 } else { fb.info.width as f32 };
        input_state.notif_curr_x += (target_notif_x - input_state.notif_curr_x) * 0.2;

        let target_launcher_y = if input_state.launcher_active { (fb.info.height as i32 - 370) as f32 } else { fb.info.height as f32 };
        input_state.launcher_curr_y += (target_launcher_y - input_state.launcher_curr_y) * 0.2;

        let target_ws_offset = (input_state.current_workspace as f32) * (fb.info.width as f32);
        input_state.workspace_offset += (target_ws_offset - input_state.workspace_offset) * 0.15;

        let target_search_y = if input_state.search_active { 0.0 } else { -(fb.info.height as f32 / 2.0) };
        input_state.search_curr_y += (target_search_y - input_state.search_curr_y) * 0.15;

        // --- Rendering ---
        if !input_state.lock_active {
            draw_static_ui(&mut fb, &windows, window_count, counter, input_state.cursor_x, input_state.cursor_y);
            
            if !input_state.dashboard_active {
                draw_shell_windows(&mut fb, &windows, window_count, input_state.focused_window, &surfaces, input_state.workspace_offset, input_state.cursor_x, input_state.cursor_y);
            } else {
                draw_dashboard(&mut fb, counter);
            }

            if input_state.quick_settings_active { draw_quick_settings(&mut fb); }
            if input_state.context_menu_active { draw_context_menu(&mut fb, input_state.context_menu_pos); }
            
            draw_launcher(&mut fb, input_state.launcher_curr_y);
            draw_notifications(&mut fb, &input_state.notifications, input_state.notif_curr_x);
            if input_state.alt_tab_active { draw_alt_tab_hud(&mut fb, &windows, window_count, input_state.focused_window); }
            if input_state.search_active || input_state.search_curr_y > -(fb.info.height as f32 / 2.0) + 5.0 {
                draw_search_hud(&mut fb, &input_state.search_query, input_state.search_selected_idx, counter, input_state.search_curr_y);
            }

            // Desktop "Stroke" drawing
            if input_state.mouse_buttons & 1 != 0 && input_state.dragging_window.is_none() {
                let d = 4u32;
                let color = STROKE_COLORS[input_state.stroke_color.min(4) as usize];
                let _ = Rectangle::new(Point::new(input_state.cursor_x, input_state.cursor_y), Size::new(d, d))
                    .into_styled(PrimitiveStyleBuilder::new().fill_color(color).build()).draw(&mut fb);
            }
        } else {
            draw_lock_screen(&mut fb, counter);
        }

        draw_cursor(&mut fb, Point::new(input_state.cursor_x, input_state.cursor_y));
        fb.present();

        if counter % 1000 == 0 {
            let used = HEAP_PTR.load(Ordering::Relaxed);
            println!("[SMITHAY] Stats: HEAP {}/8MB | IPC {} msgs", used, ipc.message_count);
        }

        if counter % 300 == 0 { for _ in 0..10 { yield_cpu(); } }
        else { yield_cpu(); }
    }
}
