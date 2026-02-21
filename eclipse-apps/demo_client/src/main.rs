#![no_std]
#![no_main]

extern crate alloc;

use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicUsize, Ordering};
use eclipse_libc::{println, yield_cpu};
use sidewind_sdk::{discover_composer, SideWindSurface};
use sidewind_sdk::ui::{self, icons, colors};
use sidewind_core::SWND_EVENT_TYPE_RESIZE;
use embedded_graphics::{
    pixelcolor::Rgb888,
    prelude::*,
    primitives::Rectangle,
};

const HEAP_SIZE: usize = 2 * 1024 * 1024; // 2MB for demo
#[repr(align(4096))]
struct Heap([u8; HEAP_SIZE]);
static mut HEAP: Heap = Heap([0u8; HEAP_SIZE]);
static HEAP_PTR: AtomicUsize = AtomicUsize::new(0);

struct StaticAllocator;
unsafe impl GlobalAlloc for StaticAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let align = layout.align();
        let size = layout.size();
        let current = HEAP_PTR.load(Ordering::SeqCst);
        let aligned = (current + align - 1) & !(align - 1);
        if aligned + size > HEAP_SIZE { return core::ptr::null_mut(); }
        HEAP_PTR.store(aligned + size, Ordering::SeqCst);
        HEAP.0.as_mut_ptr().add(aligned)
    }
    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
}

#[global_allocator]
static ALLOCATOR: StaticAllocator = StaticAllocator;

struct FramebufferTarget<'a> {
    buffer: &'a mut [u32],
    width: u32,
    height: u32,
}

impl<'a> DrawTarget for FramebufferTarget<'a> {
    type Color = Rgb888;
    type Error = core::convert::Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        for Pixel(coord, color) in pixels.into_iter() {
            if coord.x >= 0 && coord.x < self.width as i32 && coord.y >= 0 && coord.y < self.height as i32 {
                let index = coord.y as usize * self.width as usize + coord.x as usize;
                let raw = 0xFF000000 | ((color.r() as u32) << 16) | ((color.g() as u32) << 8) | color.b() as u32;
                self.buffer[index] = raw;
            }
        }
        Ok(())
    }
}

impl<'a> OriginDimensions for FramebufferTarget<'a> {
    fn size(&self) -> Size {
        Size::new(self.width, self.height)
    }
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    println!("[ECLIPSE-DEMO] Starting Aesthetic Demo Client...");

    let composer_pid = loop {
        if let Some(pid) = discover_composer() {
            println!("[ECLIPSE-DEMO] Discovered compositor at PID {}", pid);
            break pid;
        }
        yield_cpu();
    };

    let mut surface = SideWindSurface::new(composer_pid, 300, 200, 480, 360, "eclipse_demo")
        .expect("[ECLIPSE-DEMO] Failed to create surface");

    println!("[ECLIPSE-DEMO] Surface created. Using SideWind UI tokens.");

    let mut frame = 0u32;

    loop {
        // Poll events
        while let Some(ev) = surface.poll_event() {
            match ev.event_type {
                SWND_EVENT_TYPE_RESIZE => {
                    surface.set_size(ev.data1 as u32, ev.data2 as u32);
                }
                _ => {}
            }
        }

        let w = surface.width();
        let h = surface.height();
        let buffer = surface.buffer();
        
        let mut target = FramebufferTarget {
            buffer,
            width: w,
            height: h,
        };

        // 1. Background (Eclipse Deep Blue)
        let _ = target.clear(colors::BACKGROUND_DEEP);

        // 2. Center Glowing Hexagon
        let center = Point::new(w as i32 / 2, h as i32 / 2);
        let _ = ui::draw_glowing_hexagon(&mut target, center, 80, colors::ACCENT_BLUE);

        // 3. Central Icon (System)
        let _ = ui::draw_standard_icon(&mut target, center, icons::SYSTEM);

        // 4. Animated accent
        let rect_x = (frame % w) as i32;
        let _ = Rectangle::new(Point::new(rect_x, (h-5) as i32), Size::new(40, 2))
            .into_styled(embedded_graphics::primitives::PrimitiveStyle::with_fill(colors::ACCENT_BLUE))
            .draw(&mut target);

        surface.commit();
        frame = frame.wrapping_add(1);

        // Throttle to avoid maxing CPU
        for _ in 0..20 {
            yield_cpu();
        }
    }
}
