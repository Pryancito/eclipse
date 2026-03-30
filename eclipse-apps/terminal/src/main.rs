#![no_std]
#![no_main]

extern crate alloc;
extern crate eclipse_std as std;

use std::prelude::v1::*;
use alloc::rc::Rc;
use core::cell::RefCell;
use wayland_proto::wl::{ObjectId, NewId, Message, RawMessage, Interface, connection::Connection};
use wayland_proto::EclipseWaylandConnection;
use wayland_proto::wl::protocols::common::wl_registry;
use wayland_proto::wl::protocols::common::wl_display::WlDisplay;
use eclipse_syscall::{self, flag, ProcessInfo};
use heapless::String as HString;

#[cfg(target_vendor = "eclipse")]
use libc::{c_int, close, mmap, munmap, open};
#[cfg(target_vendor = "eclipse")]
use sidewind::{IpcChannel, SideWindMessage};

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
        let px = unsafe { core::slice::from_raw_parts_mut(ptr, (w as usize) * (h as usize)) };
        for (i, p) in px.iter_mut().enumerate() {
            let ww = w as usize;
            let row = i / ww;
            let content_top = 4usize;
            let content_bottom = (h as usize).saturating_sub(8);
            *p = if row >= content_top && row <= content_bottom && i % ww > 3 && i % ww + 3 < ww {
                0xFF_12_12_18
            } else {
                0xFF_0D_0D_12
            };
        }
        let commit = SideWindMessage::new_commit();
        let _ = IpcChannel::send_sidewind(lunas_pid, &commit);
        std::println!("SideWind window committed; idle loop.");
        let _ = size_bytes;
        loop {
            std::thread::yield_now();
        }
    }

    #[cfg(not(target_vendor = "eclipse"))]
    loop {
        std::thread::yield_now();
    }
}
