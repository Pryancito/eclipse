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
mod space;
mod backend;
mod state;

use state::SmithayState;
use crate::compositor::{ShellWindow, WindowContent, MAX_WINDOWS_COUNT};
use crate::compositor::ExternalSurface;
use ipc::{query_input_service_pid, subscribe_to_input_service};

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
    
    println!("[SMITHAY] Initializing Smithay Architecture...");
    let pid = getpid();
    
    let mut state = match SmithayState::new() {
        Some(s) => s,
        None => { println!("[SMITHAY] FATAL: State init failed"); loop { yield_cpu(); } }
    };
    
    state.backend.fb.pre_render_background();

    // Initial demo window
    state.space.map_window(ShellWindow {
        x: 100, y: 100, w: 400, h: 300,
        curr_x: (100 + 400/2) as f32, curr_y: (100 + 300/2) as f32, curr_w: 0.0, curr_h: 0.0,
        minimized: false, maximized: false, closing: false, stored_rect: (100, 100, 400, 300),
        workspace: 0, content: WindowContent::InternalDemo,
    });

    if let Some(in_pid) = query_input_service_pid() { subscribe_to_input_service(in_pid, pid); }

    loop {
        state.process_events();
        
        // --- Handle Window Lifecycle Requests ---
        if state.input.request_close_window && state.space.window_count > 0 {
            let idx = state.input.focused_window.unwrap_or(state.space.window_count - 1);
            state.space.windows[idx].closing = true;
            state.input.request_close_window = false;
            state.input.dragging_window = None;
            state.input.focused_window = None;
        }

        if state.input.request_new_window && state.space.window_count < MAX_WINDOWS_COUNT {
            let wc = state.space.window_count as i32;
            state.space.map_window(ShellWindow {
                x: 60 + wc * 20, y: 160 + wc * 15, w: 240, h: 180,
                curr_x: (60 + wc * 20 + 120) as f32, curr_y: (160 + wc * 15 + 90) as f32, curr_w: 0.0, curr_h: 0.0,
                minimized: false, maximized: false, closing: false, stored_rect: (60 + wc * 20, 160 + wc * 15, 240, 180),
                workspace: state.input.current_workspace, content: WindowContent::InternalDemo,
            });
            state.input.request_new_window = false;
        }
        
        state.update();
        state.render();
        state.backend.swap_buffers();

        if state.counter % 1000 == 0 {
            let used = HEAP_PTR.load(Ordering::Relaxed);
            println!("[SMITHAY] Stats: HEAP {}/8MB | IPC {} msgs", used, state.backend.ipc.message_count);
        }

        if state.counter % 300 == 0 { for _ in 0..10 { yield_cpu(); } }
        else { yield_cpu(); }
    }
}
