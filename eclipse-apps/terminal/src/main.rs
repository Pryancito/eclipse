#![no_std]
#![no_main]

extern crate alloc;
extern crate eclipse_std as std;

use std::prelude::v1::*;
use alloc::rc::Rc;
#[cfg(target_vendor = "eclipse")]
use alloc::vec::Vec;
#[cfg(target_vendor = "eclipse")]
use alloc::boxed::Box;
use core::cell::RefCell;
use wayland_proto::wl::{ObjectId, NewId, Message, RawMessage, Interface, connection::Connection};
use wayland_proto::EclipseWaylandConnection;
use wayland_proto::wl::protocols::common::wl_registry;
use wayland_proto::wl::protocols::common::wl_display::WlDisplay;
use eclipse_syscall::{self, flag, ProcessInfo};
use heapless::String as HString;

use os_terminal::{DrawTarget, Rgb, Terminal};
use os_terminal::font::BitmapFont;

#[cfg(target_vendor = "eclipse")]
use libc::{c_int, close, mmap, munmap, open};
#[cfg(target_vendor = "eclipse")]
use eclipse_ipc::prelude::EclipseMessage;
#[cfg(target_vendor = "eclipse")]
use sidewind::{IpcChannel, SideWindEvent, SideWindMessage, SWND_EVENT_TYPE_KEY};

#[cfg(target_vendor = "eclipse")]
use eclipse_syscall::call::{
    close as sys_close,
    ioctl as sys_ioctl,
    read as sys_read,
    spawn_with_stdio as sys_spawn_with_stdio,
    write as sys_write,
};

/// Nombre único de SHM en /tmp/ para SideWind (evita colisión entre procesos).
fn sidewind_shm_name(pid: u32) -> HString<24> {
    let mut s = HString::new();
    let _ = s.push_str("twb_");
    let mut n = pid;
    let mut tmp = [0u8; 10];
    let mut i = 0usize;
    if n == 0 {
        tmp[0] = b'0';
        i = 1;
    } else {
        while n > 0 && i < tmp.len() {
            tmp[i] = b'0' + (n % 10) as u8;
            n /= 10;
            i += 1;
        }
    }
    for j in 0..i / 2 {
        tmp.swap(j, i - 1 - j);
    }
    let _ = s.push_str(unsafe { core::str::from_utf8_unchecked(&tmp[..i]) });
    s
}

/// Abre `/tmp/<name>`, mapea el framebuffer y envía `SWND_OP_CREATE` / `COMMIT` al compositor.
#[cfg(target_vendor = "eclipse")]
fn open_sidewind_window(
    composer_pid: u32,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    name: &str,
) -> Option<(*mut u32, usize, u32, u32)> {
    let mut path = [0u8; 64];
    path[0..5].copy_from_slice(b"/tmp/");
    let nb = name.as_bytes();
    let nlen = nb.len().min(32).min(path.len() - 5 - 1);
    path[5..5 + nlen].copy_from_slice(&nb[..nlen]);
    path[5 + nlen] = 0;

    let size_bytes = (w as usize).saturating_mul(h as usize).saturating_mul(4);
    // Igual que `sidewind::SideWindSurface`: crear el nodo en /tmp y fijar tamaño antes de mmap.
    let fd = unsafe {
        open(
            path.as_ptr() as *const core::ffi::c_char,
            (flag::O_RDWR | flag::O_CREAT) as core::ffi::c_int,
            0o644,
        )
    };
    if fd < 0 {
        return None;
    }
    if eclipse_syscall::ftruncate(fd as usize, size_bytes).is_err() {
        unsafe {
            close(fd);
        }
        return None;
    }
    let vaddr = unsafe {
        mmap(
            core::ptr::null_mut(),
            size_bytes,
            (flag::PROT_READ | flag::PROT_WRITE) as c_int,
            flag::MAP_SHARED as c_int,
            fd,
            0,
        )
    };
    unsafe { close(fd) };
    if vaddr.is_null() || vaddr == (-1isize as *mut core::ffi::c_void) {
        return None;
    }
    let msg = SideWindMessage::new_create(x, y, w, h, name);
    if !IpcChannel::send_sidewind(composer_pid, &msg) {
        unsafe {
            munmap(vaddr, size_bytes);
        }
        return None;
    }
    Some((vaddr as *mut u32, size_bytes, w, h))
}

fn process_name_bytes(name: &[u8; 16]) -> &[u8] {
    let end = name.iter().position(|&b| b == 0).unwrap_or(16);
    &name[..end]
}

fn find_pid_by_name(want: &[u8]) -> Option<u32> {
    let mut list = [ProcessInfo::default(); 48];
    let count = eclipse_syscall::get_process_list(&mut list).ok()?;
    for info in list.iter().take(count) {
        if info.pid == 0 {
            continue;
        }
        if process_name_bytes(&info.name) == want {
            return Some(info.pid);
        }
    }
    None
}

#[cfg(target_vendor = "eclipse")]
struct SurfaceDrawTarget {
    ptr: *mut u32,
    width: usize,
    height: usize,
}

#[cfg(target_vendor = "eclipse")]
unsafe impl Send for SurfaceDrawTarget {}

#[cfg(target_vendor = "eclipse")]
impl DrawTarget for SurfaceDrawTarget {
    fn size(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    fn draw_pixel(&mut self, x: usize, y: usize, rgb: Rgb) {
        if x >= self.width || y >= self.height {
            return;
        }
        let idx = y * self.width + x;
        // Lunas lee ARGB8888 como 0xAARRGGBB (ver FramebufferState::draw_iter/blits).
        let pixel = 0xFF00_0000u32
            | ((rgb.0 as u32) << 16)
            | ((rgb.1 as u32) << 8)
            | (rgb.2 as u32);
        unsafe {
            if !self.ptr.is_null() {
                *self.ptr.add(idx) = pixel;
            }
        }
    }
}

#[no_mangle]
pub fn main() {
    std::init_runtime();

    std::println!("--- Terminal-WB (Kazari-based Wayland) ---");

    let self_pid = eclipse_syscall::getpid() as u32;
    let lunas_pid = find_pid_by_name(b"lunas").or_else(|| find_pid_by_name(b"gui"));
    let Some(lunas_pid) = lunas_pid else {
        std::println!("Compositor not found (no process named 'lunas' or 'gui').");
        return;
    };

    std::println!("Connecting to Lunas (PID {})...", lunas_pid);

    let connection = Rc::new(RefCell::new(EclipseWaylandConnection::new(lunas_pid, self_pid)));
    
    // Object 1 is always the wl_display
    let mut display = WlDisplay::new(connection.clone(), ObjectId(1));

    // Request the registry (new object ID 2)
    let registry_id = NewId(2);
    std::println!("Requesting wl_registry (ID 2)...");
    if let Err(e) = display.get_registry(registry_id) {
        std::println!("Failed to send get_registry: {:?}", e);
        return;
    }

    std::println!("Handshake sent. Waiting for events...");

    // Main loop to receive events
    let mut registry = wl_registry::WlRegistry::new(connection.clone(), ObjectId(2));
    let mut compositor_id = None;
    let mut shm_id = None;

    loop {
        let recv_res = (*connection).borrow().recv();
        match recv_res {
            Ok((data_vec, _handles)) => {
                let mut rest: &[u8] = &data_vec[..];
                while rest.len() >= 8 {
                    let (id, op, msg_len) = match RawMessage::deserialize_header(rest) {
                        Ok(h) => h,
                        Err(_) => break,
                    };
                    if msg_len > rest.len() {
                        std::println!("wl_registry: truncated frame (need {} have {})", msg_len, rest.len());
                        break;
                    }
                    let chunk = &rest[..msg_len];
                    rest = &rest[msg_len..];

                    if id == ObjectId(2) {
                        let Some(types) = wl_registry::WlRegistry::PAYLOAD_TYPES.get(op.0 as usize)
                        else {
                            std::println!("wl_registry: unknown opcode {}", op.0);
                            continue;
                        };
                        let raw = match RawMessage::deserialize(chunk, types, &[]) {
                            Ok(r) => r,
                            Err(e) => {
                                std::println!("wl_registry: decode error: {:?}", e);
                                continue;
                            }
                        };
                        if let Ok(event) = wl_registry::Event::from_raw(connection.clone(), &raw) {
                            match event {
                                wl_registry::Event::Global { name, interface, version } => {
                                    std::println!("Registry: Global {} {} v{}", name, interface, version);
                                    if interface == "wl_compositor" {
                                        let id = NewId(3);
                                        std::println!("Binding to wl_compositor (ID 3)...");
                                        if registry.bind(name, id).is_err() {
                                            std::println!("bind wl_compositor failed");
                                        }
                                        compositor_id = Some(id.as_id());
                                    } else if interface == "wl_shm" {
                                        let id = NewId(4);
                                        std::println!("Binding to wl_shm (ID 4)...");
                                        if registry.bind(name, id).is_err() {
                                            std::println!("bind wl_shm failed");
                                        }
                                        shm_id = Some(id.as_id());
                                    }
                                }
                                _ => {}
                            }
                        }
                    } else {
                        std::println!("Received message for object {:?}: Opcode={:?}", id, op);
                    }
                }
            }
            Err(e) => {
                std::println!("Recv error or timeout: {:?}", e);
            }
        }

        if compositor_id.is_some() && shm_id.is_some() {
            std::println!("Handshake complete! Both compositor and shm bound.");
            break;
        }
    }

    std::println!("Terminal initialized successfully.");

    // Wayland en Lunas aún no crea ventanas de shell por sí solo; SideWind es la ruta que el
    // compositor usa para mapear /tmp/* y mostrar un marco en el escritorio.
    #[cfg(target_vendor = "eclipse")]
    {
        let name = sidewind_shm_name(self_pid);
        let name_str = name.as_str();
        let win_w = 520u32;
        let win_h = 340u32;
        std::println!("Opening SideWind window (shm {})...", name_str);
        let Some((ptr, size_bytes, w, h)) =
            open_sidewind_window(lunas_pid, 120, 140, win_w, win_h, name_str)
        else {
            std::println!("SideWind window failed (open /tmp, mmap, or compositor IPC).");
            loop {
                std::thread::yield_now();
            }
        };

        // Render real del contenido del terminal con `os-terminal`.
        // Esto escribe en el buffer ARGB8888 mmap del shm.
        let draw_target = SurfaceDrawTarget {
            ptr,
            width: w as usize,
            height: h as usize,
        };
        let font = Box::new(BitmapFont);
        let mut terminal = Terminal::new(draw_target, font);
        terminal.set_crnl_mapping(true);

        // Mensaje inicial (visible antes de que `/bin/sh` emita su prompt).
        terminal.process(b"\x1b[1;36mEclipse OS terminal-wb\x1b[0m\r\n");
        terminal.flush();
        let _ = IpcChannel::send_sidewind(lunas_pid, &SideWindMessage::new_commit());

        // --- PTY + /bin/sh ---
        // - Nosotros escribimos al `pty:master`
        // - El shell lee del `pty:slave/<n>`
        // - Nosotros leemos la salida desde `pty:master`
        //
        // Importante: el syscall kernel asume que la ruta es C-string (terminada en NUL).
        // Por eso usamos `libc::open` con buffers NUL.
        let pty_master_fd = unsafe {
            let path = b"pty:master\0";
            let fd = open(
                path.as_ptr() as *const core::ffi::c_char,
                (flag::O_RDWR | flag::O_CREAT) as c_int,
                0,
            );
            if fd < 0 {
                -1isize as usize
            } else {
                fd as usize
            }
        };
        if pty_master_fd == (-1isize as usize) {
            std::println!("Failed to open pty:master");
            loop {
                std::thread::yield_now();
            }
        }

        // Obtener el índice del PTY para abrir el slave correspondiente.
        let mut pair_id: usize = 0;
        let _ = sys_ioctl(
            pty_master_fd,
            1, // request 1: TIOCGPTN in this pty implementation
            &mut pair_id as *mut usize as usize,
        );

        let slave_path = alloc::format!("pty:slave/{}", pair_id);
        let pty_slave_fd = unsafe {
            let mut path_buf = [0u8; 64];
            let bytes = slave_path.as_bytes();
            let n = bytes.len().min(path_buf.len() - 1);
            path_buf[..n].copy_from_slice(&bytes[..n]);
            path_buf[n] = 0;
            let fd = open(
                path_buf.as_ptr() as *const core::ffi::c_char,
                flag::O_RDWR as c_int,
                0,
            );
            if fd < 0 {
                -1isize as usize
            } else {
                fd as usize
            }
        };
        if pty_slave_fd == (-1isize as usize) {
            std::println!("Failed to open {}", slave_path);
            let _ = sys_close(pty_master_fd).ok();
            loop {
                std::thread::yield_now();
            }
        }

        // Cargar /bin/sh como ELF y spawnear con stdio conectado al PTY slave.
        let sh_fd = unsafe {
            let path = b"/bin/sh\0";
            let fd = open(
                path.as_ptr() as *const core::ffi::c_char,
                flag::O_RDONLY as c_int,
                0,
            );
            if fd < 0 {
                -1isize as usize
            } else {
                fd as usize
            }
        };
        if sh_fd == (-1isize as usize) {
            std::println!("Failed to open /bin/sh");
            let _ = sys_close(pty_slave_fd).ok();
            let _ = sys_close(pty_master_fd).ok();
            loop {
                std::thread::yield_now();
            }
        }

        let mut sh_bytes: Vec<u8> = Vec::with_capacity(128 * 1024);
        let mut read_buf = [0u8; 4096];
        loop {
            let n = sys_read(sh_fd, &mut read_buf).unwrap_or(0);
            if n == 0 {
                break;
            }
            sh_bytes.extend_from_slice(&read_buf[..n]);
        }
        let _ = sys_close(sh_fd);

        let _sh_pid = match sys_spawn_with_stdio(
            &sh_bytes,
            Some("sh"),
            pty_slave_fd,
            pty_slave_fd,
            pty_slave_fd,
        ) {
            Ok(pid) => pid,
            Err(_) => {
                std::println!("Failed to spawn sh (PTY)");
                loop {
                    std::thread::yield_now();
                }
            }
        };

        // Conectar escritor de PTY para `os-terminal` (bytes del teclado -> PTY master).
        terminal.set_auto_flush(false);
        let pfd_for_writer = pty_master_fd;
        terminal.set_pty_writer(Box::new(move |s: &str| {
            let _ = sys_write(pfd_for_writer, s.as_bytes());
        }));

        // --- Loop interactivo ---
        // Teclado: Lunas reenvía `SideWindEvent` KEY al PID del cliente (no abrir `input:`: compite con el compositor).
        let mut ipc_ch = IpcChannel::new();
        let mut pty_buf = [0u8; 1024];
        let sw_ev_sz = core::mem::size_of::<SideWindEvent>();

        std::println!("Terminal interactive: teclado (IPC desde Lunas) + shell (PTY).");
        let _ = size_bytes;
        loop {
            let mut dirty = false;

            // 1) Teclado: drenar mensajes IPC (eventos KEY del compositor).
            while let Some(msg) = ipc_ch.recv() {
                if let EclipseMessage::Raw { data, len, .. } = msg {
                    if len == sw_ev_sz {
                        let ev = unsafe {
                            core::ptr::read_unaligned(data.as_ptr() as *const SideWindEvent)
                        };
                        if ev.event_type == SWND_EVENT_TYPE_KEY {
                            let sc = (ev.data1 as u16 & 0xFF) as u8;
                            let pressed = ev.data2 != 0;
                            let ps2 = if pressed { sc } else { sc | 0x80 };
                            let _ = terminal.handle_keyboard(ps2);
                            dirty = true;
                        }
                    }
                }
            }

            // 2) Salida del PTY: drenar bytes disponibles
            let mut available: usize = 0;
            let _ = sys_ioctl(
                pty_master_fd,
                2, // request 2: FIONREAD (bytes disponibles)
                &mut available as *mut usize as usize,
            );

            while available > 0 {
                let to_read = available.min(pty_buf.len());
                let n = sys_read(pty_master_fd, &mut pty_buf[..to_read]).unwrap_or(0);
                if n == 0 {
                    break;
                }
                terminal.process(&pty_buf[..n]);
                dirty = true;

                // Re-leer available para drenar en lotes.
                available = 0;
                let _ = sys_ioctl(
                    pty_master_fd,
                    2,
                    &mut available as *mut usize as usize,
                );
            }

            // 3) Solo flushear + commit si hubo cambios reales.
            if dirty {
                terminal.flush();
                let _ = IpcChannel::send_sidewind(lunas_pid, &SideWindMessage::new_commit());
            } else {
                // Evitar 100% CPU: ceder al scheduler cuando estamos idle.
                let _ = eclipse_syscall::call::sched_yield();
                std::thread::yield_now();
            }
        }
    }

    #[cfg(not(target_vendor = "eclipse"))]
    loop {
        std::thread::yield_now();
    }
}
