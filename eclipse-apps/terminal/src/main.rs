#![cfg_attr(target_vendor = "eclipse", no_std)]

#[cfg(target_vendor = "eclipse")]
extern crate alloc;
#[cfg(target_vendor = "eclipse")]
extern crate eclipse_std as std;

use os_terminal::{DrawTarget, Terminal, Rgb};
use os_terminal::font::{TrueTypeFont};
use alloc::boxed::Box;

// Usamos únicamente la librería base mocked en Sidewind
use sidewind::wayland_client::{
    protocol::{
        wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface,
    },
    Connection, Dispatch, QueueHandle,
};

const WIDTH: usize = 640;
const HEIGHT: usize = 480;
const FONT_DATA: &[u8] = include_bytes!("../../../libcosmic/res/noto/NotoSansMono-Regular.ttf");

// --- 1. NUESTRO ESTADO GLOBAL ---
struct AppState {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    surface: Option<wl_surface::WlSurface>,
    buffer: Option<wl_buffer::WlBuffer>,
    
    // Terminal logic
    terminal: Option<Terminal<WaylandDrawTarget<'static>>>,
    font_size: f32,
    master_fd: usize,
    vaddr: *mut u32,
}

// --- 2. LA MÁQUINA DE ESTADOS (DISPATCHERS) ---

impl Dispatch<wl_registry::WlRegistry, ()> for AppState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<AppState>,
    ) {
        let wl_registry::Event::Global { name, interface, .. } = event;
        match &interface[..] {
            "wl_compositor" => {
                state.compositor = Some(registry.bind::<wl_compositor::WlCompositor, _, _>(
                    name, 1, qh, ()
                ));
            }
            "wl_shm" => {
                state.shm = Some(registry.bind::<wl_shm::WlShm, _, _>(
                    name, 1, qh, ()
                ));
            }
            _ => {}
        }
    }
}

// Implementaciones vacías para cumplir con el trait (como en la referencia)
impl Dispatch<wl_compositor::WlCompositor, ()> for AppState {
    fn event(_: &mut Self, _: &wl_compositor::WlCompositor, _: (), _: &(), _: &Connection, _: &QueueHandle<AppState>) {}
}
impl Dispatch<wl_shm::WlShm, ()> for AppState {
    fn event(_: &mut Self, _: &wl_shm::WlShm, _: (), _: &(), _: &Connection, _: &QueueHandle<AppState>) {}
}
impl Dispatch<wl_surface::WlSurface, ()> for AppState {
    fn event(_: &mut Self, _: &wl_surface::WlSurface, _: (), _: &(), _: &Connection, _: &QueueHandle<AppState>) {}
}
impl Dispatch<wl_shm_pool::WlShmPool, ()> for AppState {
    fn event(_: &mut Self, _: &wl_shm_pool::WlShmPool, _: (), _: &(), _: &Connection, _: &QueueHandle<AppState>) {}
}
impl Dispatch<wl_buffer::WlBuffer, ()> for AppState {
    fn event(_: &mut Self, _: &wl_buffer::WlBuffer, _: (), _: &(), _: &Connection, _: &QueueHandle<AppState>) {}
}

// Target de renderizado manual para os-terminal
struct WaylandDrawTarget<'a> {
    buffer: &'a mut [u8],
    width: usize,
    height: usize,
}

impl<'a> DrawTarget for WaylandDrawTarget<'a> {
    fn size(&self) -> (usize, usize) { (self.width, self.height) }
    #[inline(always)]
    fn draw_pixel(&mut self, x: usize, y: usize, color: Rgb) {
        if x < self.width && y < self.height {
            let offset = (y * self.width + x) * 4;
            self.buffer[offset] = color.2;     // B
            self.buffer[offset + 1] = color.1; // G
            self.buffer[offset + 2] = color.0; // R
            self.buffer[offset + 3] = 0xFF;    // A
        }
    }
}

#[cfg(target_vendor = "eclipse")]
fn main() {
    use std::prelude::v1::*;
    use eclipse_syscall::call::{open, mmap, write, read, ioctl, sched_yield};
    use eclipse_syscall::flag::{O_RDWR, O_CREAT, PROT_READ, PROT_WRITE, MAP_SHARED};

    let stride = WIDTH * 4;
    let size_bytes = stride * HEIGHT;

    // 1. Conectar al compositor
    let conn = Connection::connect_to_env().expect("Sidewind no responde");
    let display = conn.display();
    let mut event_queue = conn.new_event_queue();
    let qh: QueueHandle<AppState> = event_queue.handle();

    let mut state = AppState {
        compositor: None,
        shm: None,
        surface: None,
        buffer: None,
        terminal: None,
        font_size: 14.0,
        master_fd: 0,
        vaddr: core::ptr::null_mut(),
    };

    // 2. Obtener el Registry y sincronizar
    let registry = display.get_registry(&qh, ());
    event_queue.roundtrip(&mut state).expect("Registry sync failed");

    let compositor = state.compositor.as_ref().expect("No compositor");
    let shm = state.shm.as_ref().expect("No shm");

    // 3. Crear Superficie
    let surface = compositor.create_surface(&qh, ());
    state.surface = Some(surface.clone());

    // 4. Preparar memoria compartida (SHM) - Estilo Eclipse
    let fd = open("shm:Terminal", O_RDWR | O_CREAT).expect("Failed to open SHM");
    let vaddr = mmap(0, size_bytes, PROT_READ | PROT_WRITE, MAP_SHARED, fd as isize, 0).expect("Failed to mmap");
    state.vaddr = vaddr as *mut u32;

    // 5. Crear Pool y Buffer
    let pool = shm.create_pool(fd as i32, size_bytes as i32, &qh, ());
    let buffer = pool.create_buffer(0, WIDTH as i32, HEIGHT as i32, stride as i32, wl_shm::Format::Argb8888, &qh, ());
    state.buffer = Some(buffer);

    // 6. Inicializar PTY y Terminal Logic
    let master_fd = open("pty:master", O_RDWR).expect("Failed to open pty master");
    state.master_fd = master_fd;
    
    // Launch shell (Simulado o real si existe spawn)
    // let _ = spawn(b"/bin/sh", None);

    let bytes = unsafe { core::slice::from_raw_parts_mut(vaddr as *mut u8, size_bytes) };
    let display_target = WaylandDrawTarget { buffer: bytes, width: WIDTH, height: HEIGHT };
    let mut terminal = Terminal::new(display_target, Box::new(TrueTypeFont::new(state.font_size, FONT_DATA)));
    terminal.set_crnl_mapping(true);
    
    let mfd = master_fd;
    terminal.set_pty_writer(Box::new(move |s: &str| {
        let _ = write(mfd, s.as_bytes());
    }));
    
    terminal.process(b"=== Raw Wayland Terminal ===\r\n");
    terminal.flush();
    state.terminal = Some(terminal);

    // 7. Attach and Commit
    surface.attach(state.buffer.as_ref(), 0, 0);
    surface.damage_buffer(0, 0, WIDTH as i32, HEIGHT as i32);
    surface.commit();

    // 8. Bucle infinito (Dispatch)
    loop {
        // A) Procesar eventos Wayland (Teclado, etc.)
        let _ = event_queue.blocking_dispatch(&mut state);

        // B) Lógica PTY (Manual)
        if let Some(term) = &mut state.terminal {
            let mut available: usize = 0;
            let res = ioctl(master_fd, 2, &mut available as *mut usize as usize);
            if res.is_ok() && available > 0 {
                let mut pty_buf = [0u8; 512];
                if let Ok(n) = read(master_fd, &mut pty_buf) {
                    if n > 0 {
                        term.process(&pty_buf[..n]);
                        term.flush();
                        
                        // Re-commit
                        surface.commit();
                    }
                }
            }
        }
        
        let _ = sched_yield();
    }
}

#[cfg(not(target_vendor = "eclipse"))]
fn main() { println!("Eclipse OS only."); }
