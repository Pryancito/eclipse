#![no_std]

extern crate alloc;
use alloc::vec::Vec;
use alloc::collections::VecDeque;

/// Wayland wire protocol message header
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct WaylandHeader {
    pub object_id: u32,
    pub size_and_opcode: u32,
}

pub struct WaylandConnection {
    pub registry: ObjectRegistry,
    pub pending_events: VecDeque<Vec<u8>>,
}

impl WaylandConnection {
    pub fn new() -> Self {
        Self {
            registry: ObjectRegistry::new(),
            pending_events: VecDeque::new(),
        }
    }

    pub fn send_event(&mut self, object_id: u32, opcode: u16, args: &[u8]) {
        let size = 8 + args.len();
        let mut data = Vec::with_capacity(size);
        data.extend_from_slice(&object_id.to_le_bytes());
        let size_op = ((size as u32) << 16) | (opcode as u32);
        data.extend_from_slice(&size_op.to_le_bytes());
        data.extend_from_slice(args);
        while data.len() % 4 != 0 {
            data.push(0);
        }
        self.pending_events.push_back(data);
    }

    pub fn process_message(&mut self, data: &[u8]) -> Option<Vec<u8>> {
        if data.len() < 8 {
            return None;
        }
        let mut obj_id_bytes = [0u8; 4];
        obj_id_bytes.copy_from_slice(&data[0..4]);
        let obj_id = u32::from_le_bytes(obj_id_bytes);
        let mut size_op_bytes = [0u8; 4];
        size_op_bytes.copy_from_slice(&data[4..8]);
        let size_op = u32::from_le_bytes(size_op_bytes);
        let size = (size_op >> 16) as usize;
        let opcode = (size_op & 0xFFFF) as u16;
        if data.len() < size {
            return None;
        }
        let args = &data[8..size];
        let mut event_intent: Option<(u32, u16, Vec<u8>)> = None;
        let mut bind_intent: Option<(u32, alloc::boxed::Box<dyn WaylandObject>)> = None;
        let mut res: Option<Vec<u8>> = None;
        if let Some(obj) = self.registry.get_mut(obj_id) {
            let iface = obj.interface_name();
            let _ = obj.handle_request(opcode, args);
            if obj_id == 1 && opcode == 1 {
                let (decoded, _) = decode_args("n", args);
                if let Some(WaylandArg::NewId(new_id)) = decoded.get(0) {
                    let id = *new_id;
                    bind_intent = Some((id, alloc::boxed::Box::new(objects::WlRegistry)));
                    let mut event_args = Vec::new();
                    event_args.extend_from_slice(&1u32.to_le_bytes());
                    let ifname = b"wl_compositor\0";
                    let len = ifname.len() as u32;
                    event_args.extend_from_slice(&len.to_le_bytes());
                    event_args.extend_from_slice(ifname);
                    while event_args.len() % 4 != 0 {
                        event_args.push(0);
                    }
                    event_args.extend_from_slice(&1u32.to_le_bytes());
                    event_intent = Some((id, 0, event_args));
                }
            }
            if iface == "wl_compositor" && opcode == 0 {
                let (decoded, _) = decode_args("n", args);
                if let Some(WaylandArg::NewId(new_id)) = decoded.get(0) {
                    bind_intent = Some((*new_id, alloc::boxed::Box::new(objects::WlSurface)));
                    res = Some(new_id.to_le_bytes().to_vec());
                }
            }
            if iface == "wl_registry" && opcode == 0 {
                let (decoded, _) = decode_args("usun", args);
                if let (Some(WaylandArg::String(iface)), Some(WaylandArg::NewId(new_id))) =
                    (decoded.get(1), decoded.get(3))
                {
                    if iface == b"wl_compositor" {
                        bind_intent =
                            Some((*new_id, alloc::boxed::Box::new(objects::WlCompositor)));
                    }
                }
            }
        }
        if let Some((id, op, args)) = event_intent {
            self.send_event(id, op, &args);
        }
        if let Some((id, obj)) = bind_intent {
            self.registry.set(id, obj);
        }
        res
    }
}

impl WaylandHeader {
    pub fn size(&self) -> u16 {
        (self.size_and_opcode >> 16) as u16
    }
    pub fn opcode(&self) -> u16 {
        (self.size_and_opcode & 0xFFFF) as u16
    }
    pub fn new(object_id: u32, opcode: u16, size: u16) -> Self {
        Self {
            object_id,
            size_and_opcode: ((size as u32) << 16) | (opcode as u32),
        }
    }
}

pub trait WaylandObject {
    fn interface_name(&self) -> &'static str;
    fn version(&self) -> u32;
    fn handle_request(&mut self, opcode: u16, args: &[u8]) -> Option<Vec<u8>>;
}

#[derive(Debug, Clone, PartialEq)]
pub enum WaylandArg {
    Int(i32),
    Uint(u32),
    Fixed(i32),
    String(Vec<u8>),
    Object(u32),
    NewId(u32),
    Array(Vec<u8>),
    Fd(i32),
}

pub fn decode_args(signature: &str, data: &[u8]) -> (Vec<WaylandArg>, usize) {
    let mut args = Vec::new();
    let mut offset = 0;
    for c in signature.chars() {
        match c {
            'i' | 'h' => {
                let mut val = [0u8; 4];
                val.copy_from_slice(&data[offset..offset + 4]);
                args.push(if c == 'i' {
                    WaylandArg::Int(i32::from_le_bytes(val))
                } else {
                    WaylandArg::Fd(i32::from_le_bytes(val))
                });
                offset += 4;
            }
            'u' | 'o' | 'n' => {
                let mut val = [0u8; 4];
                val.copy_from_slice(&data[offset..offset + 4]);
                let u = u32::from_le_bytes(val);
                args.push(match c {
                    'u' => WaylandArg::Uint(u),
                    'o' => WaylandArg::Object(u),
                    'n' => WaylandArg::NewId(u),
                    _ => unreachable!(),
                });
                offset += 4;
            }
            's' => {
                let mut len_bytes = [0u8; 4];
                len_bytes.copy_from_slice(&data[offset..offset + 4]);
                let len = u32::from_le_bytes(len_bytes) as usize;
                offset += 4;
                if len > 0 {
                    let s = data[offset..offset + len - 1].to_vec();
                    args.push(WaylandArg::String(s));
                    offset += (len + 3) & !3;
                } else {
                    args.push(WaylandArg::String(Vec::new()));
                }
            }
            'f' => {
                let mut val = [0u8; 4];
                val.copy_from_slice(&data[offset..offset + 4]);
                args.push(WaylandArg::Fixed(i32::from_le_bytes(val)));
                offset += 4;
            }
            'a' => {
                let mut len_bytes = [0u8; 4];
                len_bytes.copy_from_slice(&data[offset..offset + 4]);
                let len = u32::from_le_bytes(len_bytes) as usize;
                offset += 4;
                let a = data[offset..offset + len].to_vec();
                args.push(WaylandArg::Array(a));
                offset += (len + 3) & !3;
            }
            _ => {}
        }
    }
    (args, offset)
}

pub struct ObjectRegistry {
    pub objects: Vec<Option<alloc::boxed::Box<dyn WaylandObject>>>,
}

impl ObjectRegistry {
    pub fn new() -> Self {
        let mut reg = Self {
            objects: Vec::new(),
        };
        reg.set(1, alloc::boxed::Box::new(objects::WlDisplay::new()));
        reg
    }
    pub fn get_mut(&mut self, id: u32) -> Option<&mut (dyn WaylandObject + 'static)> {
        if id == 0 || id as usize > self.objects.len() {
            return None;
        }
        self.objects[id as usize - 1].as_deref_mut()
    }
    pub fn set(&mut self, id: u32, obj: alloc::boxed::Box<dyn WaylandObject>) {
        let idx = id as usize - 1;
        if idx >= self.objects.len() {
            self.objects.resize_with(idx + 1, || None);
        }
        self.objects[idx] = Some(obj);
    }
}

pub mod objects {
    use super::*;
    pub struct WlDisplay {
        callback_id: u32,
    }
    impl WlDisplay {
        pub fn new() -> Self {
            Self { callback_id: 0 }
        }
    }
    impl WaylandObject for WlDisplay {
        fn interface_name(&self) -> &'static str {
            "wl_display"
        }
        fn version(&self) -> u32 {
            1
        }
        fn handle_request(&mut self, opcode: u16, args: &[u8]) -> Option<Vec<u8>> {
            match opcode {
                0 => None,
                1 => {
                    let (decoded, _) = decode_args("n", args);
                    if let Some(WaylandArg::NewId(id)) = decoded.get(0) {
                        return Some(id.to_le_bytes().to_vec());
                    }
                    None
                }
                _ => None,
            }
        }
    }
    pub struct WlRegistry;
    impl WaylandObject for WlRegistry {
        fn interface_name(&self) -> &'static str {
            "wl_registry"
        }
        fn version(&self) -> u32 {
            1
        }
        fn handle_request(&mut self, opcode: u16, _args: &[u8]) -> Option<Vec<u8>> {
            match opcode {
                0 => None,
                _ => None,
            }
        }
    }
    pub struct WlCompositor;
    impl WaylandObject for WlCompositor {
        fn interface_name(&self) -> &'static str {
            "wl_compositor"
        }
        fn version(&self) -> u32 {
            4
        }
        fn handle_request(&mut self, opcode: u16, args: &[u8]) -> Option<Vec<u8>> {
            match opcode {
                0 => {
                    let (decoded, _) = decode_args("n", args);
                    if let Some(WaylandArg::NewId(id)) = decoded.get(0) {
                        return Some(id.to_le_bytes().to_vec());
                    }
                    None
                }
                _ => None,
            }
        }
    }
    pub struct WlSurface;
    impl WaylandObject for WlSurface {
        fn interface_name(&self) -> &'static str {
            "wl_surface"
        }
        fn version(&self) -> u32 {
            4
        }
        fn handle_request(&mut self, _opcode: u16, _args: &[u8]) -> Option<Vec<u8>> {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{WaylandConnection, WaylandHeader, WaylandArg, decode_args};
    #[test]
    fn test_stress_wayland_header() {
        const ITERS: u32 = 100_000;
        for i in 0..ITERS {
            let h = WaylandHeader::new((i % 256) as u32, (i % 256) as u16, 12);
            assert_eq!(h.size(), 12);
            assert_eq!(h.opcode(), (i % 256) as u16);
        }
    }
    #[test]
    fn test_stress_send_event() {
        const ITERS: u32 = 20_000;
        let mut conn = WaylandConnection::new();
        for i in 0..ITERS {
            let args = (i as u32).to_le_bytes();
            conn.send_event(2, (i % 64) as u16, &args);
        }
        assert_eq!(conn.pending_events.len(), ITERS as usize);
    }
    #[test]
    fn test_stress_decode_args_u() {
        const ITERS: u32 = 50_000;
        let data = [0x78, 0x56, 0x34, 0x12];
        for _ in 0..ITERS {
            let (decoded, _) = decode_args("u", &data);
            if let Some(WaylandArg::Uint(u)) = decoded.get(0) {
                assert_eq!(*u, 0x12345678);
            }
        }
    }
}
