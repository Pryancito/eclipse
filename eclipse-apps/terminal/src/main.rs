use std::rc::Rc;
use std::vec::Vec;
use std::boxed::Box;
use core::cell::{Ref, RefCell};
use wayland_proto::wl::{ObjectId, NewId, Message, RawMessage, Interface, connection::Connection};
use wayland_proto::EclipseWaylandConnection;
use wayland_proto::wl::protocols::common::wl_registry;
use wayland_proto::wl::protocols::common::wl_display::WlDisplay;
use eclipse_syscall::{self, flag, ProcessInfo};
use heapless::String as HString;

use os_terminal::{ClipboardHandler, DrawTarget, MouseInput, Rgb, Terminal};
use os_terminal::font::BitmapFont;

/// Portapapeles en memoria para el terminal de Eclipse OS.
/// No hay portapapeles del sistema, pero Ctrl+Shift+C / Ctrl+Shift+V
/// funcionan dentro de la misma sesión del terminal.
#[cfg(target_vendor = "eclipse")]
struct EclipseClipboard {
    text: std::string::String,
}

#[cfg(target_vendor = "eclipse")]
impl EclipseClipboard {
    fn new() -> Self {
        Self { text: std::string::String::new() }
    }
}

#[cfg(target_vendor = "eclipse")]
impl ClipboardHandler for EclipseClipboard {
    fn get_text(&mut self) -> Option<std::string::String> {
        if self.text.is_empty() { None } else { Some(self.text.clone()) }
    }
    fn set_text(&mut self, text: std::string::String) {
        self.text = text;
    }
}

#[cfg(target_vendor = "eclipse")]
use libc::{c_int, close, kill, mmap, munmap, open};
#[cfg(target_vendor = "eclipse")]
use eclipse_ipc::prelude::EclipseMessage;
#[cfg(target_vendor = "eclipse")]
use sidewind::{
    IpcChannel, SideWindEvent, SideWindMessage,
    SWND_EVENT_TYPE_KEY, SWND_EVENT_TYPE_CLOSE, SWND_EVENT_TYPE_RESIZE,
    SWND_EVENT_TYPE_MOUSE_BUTTON,
};

#[cfg(target_vendor = "eclipse")]
use eclipse_syscall::call::{
    close as sys_close,
    exit as sys_exit,
    ioctl as sys_ioctl,
    read as sys_read,
    spawn_with_stdio as sys_spawn_with_stdio,
    write as sys_write,
};

/// Escanea un buffer de bytes del PTY buscando la última secuencia OSC de título.
/// Formato: ESC ] {0|1|2} ; <título> { BEL(\x07) | ST(\x1b\\) }
/// Devuelve el título en un array de 32 bytes (con NUL al final) o None.
#[cfg(target_vendor = "eclipse")]
fn extract_osc_title(buf: &[u8]) -> Option<[u8; 32]> {
    let mut last: Option<[u8; 32]> = None;
    let mut i = 0;
    while i + 3 < buf.len() {
        if buf[i] == b'\x1b' && buf[i + 1] == b']' {
            // Aceptamos parámetros 0 (icon+title), 1 (icon) y 2 (title)
            if matches!(buf[i + 2], b'0' | b'1' | b'2') && buf.get(i + 3) == Some(&b';') {
                let ts = i + 4; // primer byte del texto
                let mut j = ts;
                while j < buf.len() {
                    let term_bel = buf[j] == b'\x07';
                    let term_st  = buf[j] == b'\x1b' && buf.get(j + 1) == Some(&b'\\');
                    if term_bel || term_st {
                        let len = (j - ts).min(31);
                        let mut t = [0u8; 32];
                        t[..len].copy_from_slice(&buf[ts..ts + len]);
                        last = Some(t);
                        i = j + if term_bel { 1 } else { 2 };
                        break;
                    }
                    j += 1;
                }
                if j == buf.len() {
                    break; // secuencia incompleta, esperar al próximo chunk
                }
            } else {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    last
}

/// Tamaño de celda del BitmapFont (noto-sans-mono-bitmap, talla por defecto).
/// Se usa para calcular cols/rows a partir de las dimensiones en píxeles.
#[cfg(target_vendor = "eclipse")]
const FONT_CHAR_W: u16 = 8;
#[cfg(target_vendor = "eclipse")]
const FONT_CHAR_H: u16 = 16;

/// Layout de `struct winsize` (POSIX):
///   [ws_rows, ws_cols, ws_xpixel, ws_ypixel]  —  4 × u16 = 8 bytes
#[cfg(target_vendor = "eclipse")]
#[repr(C)]
struct WinSize {
    ws_rows:   u16,
    ws_cols:   u16,
    ws_xpixel: u16,
    ws_ypixel: u16,
}

#[cfg(target_vendor = "eclipse")]
fn set_pty_winsize(pty_master_fd: usize, rows: u16, cols: u16, xpix: u16, ypix: u16) {
    let ws = WinSize { ws_rows: rows, ws_cols: cols, ws_xpixel: xpix, ws_ypixel: ypix };
    let _ = sys_ioctl(pty_master_fd, 3, &ws as *const WinSize as usize);
}

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

/// Buffer mmap del cliente; se sustituye entero al redimensionar (nuevo SHM + CREATE a Lunas).
#[cfg(target_vendor = "eclipse")]
struct SurfaceBacking {
    ptr: *mut u32,
    width: usize,
    height: usize,
    size_bytes: usize,
}

#[cfg(target_vendor = "eclipse")]
struct SharedSurfaceDrawTarget {
    state: Rc<RefCell<SurfaceBacking>>,
}

#[cfg(target_vendor = "eclipse")]
unsafe impl Send for SharedSurfaceDrawTarget {}

#[cfg(target_vendor = "eclipse")]
impl DrawTarget for SharedSurfaceDrawTarget {
    fn size(&self) -> (usize, usize) {
        let b: Ref<'_, SurfaceBacking> = RefCell::borrow(&*self.state);
        (b.width, b.height)
    }

    fn draw_pixel(&mut self, x: usize, y: usize, rgb: Rgb) {
        let b: Ref<'_, SurfaceBacking> = RefCell::borrow(&*self.state);
        if x >= b.width || y >= b.height {
            return;
        }
        let idx = y * b.width + x;
        let pixel = 0xFF00_0000u32
            | ((rgb.0 as u32) << 16)
            | ((rgb.1 as u32) << 8)
            | (rgb.2 as u32);
        unsafe {
            if !b.ptr.is_null() {
                *b.ptr.add(idx) = pixel;
            }
        }
    }
}

fn main() {

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

        let surface_state = Rc::new(RefCell::new(SurfaceBacking {
            ptr,
            width: w as usize,
            height: h as usize,
            size_bytes,
        }));

        let draw_target = SharedSurfaceDrawTarget {
            state: surface_state.clone(),
        };
        let font = Box::new(BitmapFont);
        let mut terminal = Terminal::new(draw_target, font);
        terminal.set_crnl_mapping(true);
        terminal.set_clipboard(Box::new(EclipseClipboard::new()));

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

        let slave_path = std::format!("pty:slave/{}", pair_id);
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

        let sh_pid = match sys_spawn_with_stdio(
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

        // Informar al PTY del tamaño inicial de la ventana para que programas
        // como vim/htop/nano sepan cuántas columnas y filas tienen disponibles.
        let init_cols = win_w as u16 / FONT_CHAR_W;
        let init_rows = win_h as u16 / FONT_CHAR_H;
        set_pty_winsize(pty_master_fd, init_rows, init_cols, win_w as u16, win_h as u16);
        std::println!("PTY window size: {}x{} chars ({}x{} px)", init_cols, init_rows, win_w, win_h);

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
        // Último título enviado a Lunas; evita mensajes redundantes.
        let mut last_title: [u8; 32] = [0; 32];

        std::println!("Terminal interactive: teclado (IPC desde Lunas) + shell (PTY).");
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
                        } else if ev.event_type == SWND_EVENT_TYPE_MOUSE_BUTTON {
                            // Botones de rueda del ratón: 4 = scroll up, 5 = scroll down.
                            // (Convenio X11; los botones 1/2/3 se ignoran por ahora.)
                            match ev.data1 {
                                4 => {
                                    // Rueda hacia el usuario → mostrar historial (scroll up).
                                    terminal.handle_mouse(MouseInput::Scroll(3));
                                    dirty = true;
                                }
                                5 => {
                                    // Rueda alejándose → avanzar al final (scroll down).
                                    terminal.handle_mouse(MouseInput::Scroll(-3));
                                    dirty = true;
                                }
                                _ => {}
                            }
                        } else if ev.event_type == SWND_EVENT_TYPE_RESIZE {
                            // Área de contenido en px (Lunas: data2 ya excluye la barra de título).
                            let new_w = ev.data1.max(1) as u32;
                            let new_h = ev.data2.max(1) as u32;
                            {
                                let sb = RefCell::borrow_mut(&*surface_state);
                                unsafe {
                                    munmap(sb.ptr as *mut core::ffi::c_void, sb.size_bytes);
                                }
                            }
                            if let Some((ptr, size_bytes, w, h)) =
                                open_sidewind_window(lunas_pid, 0, 0, new_w, new_h, name_str)
                            {
                                *RefCell::borrow_mut(&*surface_state) = SurfaceBacking {
                                    ptr,
                                    width: w as usize,
                                    height: h as usize,
                                    size_bytes,
                                };
                                let new_cols = new_w as u16 / FONT_CHAR_W;
                                let new_rows = new_h as u16 / FONT_CHAR_H;
                                set_pty_winsize(
                                    pty_master_fd,
                                    new_rows,
                                    new_cols,
                                    new_w as u16,
                                    new_h as u16,
                                );
                                // Recalcula rejilla de celdas según el nuevo `DrawTarget::size()`.
                                terminal.set_font_manager(Box::new(BitmapFont));
                                std::println!(
                                    "[TERM] resize → {}x{} chars ({}x{} px)",
                                    new_cols, new_rows, new_w, new_h
                                );
                                dirty = true;
                            } else {
                                std::println!("[TERM] resize: falló remap SHM / CREATE");
                            }
                        } else if ev.event_type == SWND_EVENT_TYPE_CLOSE {
                            // El usuario cerró la ventana: matar al shell y salir.
                            unsafe { kill(sh_pid as c_int, 9) };
                            let _ = sys_close(pty_slave_fd);
                            let _ = sys_close(pty_master_fd);
                            sys_exit(0);
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

                // Detectar cambios de título de ventana (secuencias OSC).
                if let Some(title) = extract_osc_title(&pty_buf[..n]) {
                    if title != last_title {
                        let _ = IpcChannel::send_sidewind(
                            lunas_pid,
                            &SideWindMessage::new_set_title(&title),
                        );
                        last_title = title;
                    }
                }

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