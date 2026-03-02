#![cfg_attr(not(test), no_std)]
#![cfg_attr(not(test), no_main)]

extern crate alloc;
extern crate eclipse_syscall;

use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicUsize, Ordering};
use eclipse_libc::{println, getpid, yield_cpu, sleep_ms, exit};

use smithay_app::state::SmithayState;
use smithay_app::compositor::{ShellWindow, WindowContent};
use smithay_app::ipc::{query_input_service_pid, subscribe_to_input_service, query_network_service_pid};

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
        None => { println!("[SMITHAY] FATAL: State init failed, exiting for watchdog restart"); exit(1); }
    };
    
    state.backend.fb.pre_render_background();

    // Initial demo window removed as requested

    if let Some(in_pid) = query_input_service_pid() { subscribe_to_input_service(in_pid, pid); }
    if let Some(net_pid) = query_network_service_pid() { state.network_pid = Some(net_pid); }

    loop {
        // Drenar todo el input pendiente antes de update/render para no llenar el mailbox del kernel
        state.process_events();

        state.update();
        state.render();
        state.backend.swap_buffers();

        // Un drenado rápido tras render por si llegaron eventos durante el frame
        state.process_events();

        // Intentar mapear el framebuffer si estamos en modo headless (cada ~5s a ~60fps)
        if state.counter % 300 == 0 {
            state.backend.fb.try_remap_framebuffer();
        }

        if state.counter % 1000 == 0 {
            let used = HEAP_PTR.load(Ordering::Relaxed);
            println!("[SMITHAY] Stats: HEAP {}/8MB | IPC {} msgs.",
                used, state.backend.ipc.message_count);
        }

        yield_cpu();
    }
}
