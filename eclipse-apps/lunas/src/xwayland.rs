//! X11 server for Eclipse OS (XWayland compatibility layer).
use std::prelude::v1::*;
use std::collections::BTreeMap;

// ── libc bindings ─────────────────────────────────────────────────────────────
extern "C" {
    fn socket(domain: i32, type_: i32, protocol: i32) -> i32;
    fn bind(fd: i32, addr: *const SockaddrUn, addrlen: u32) -> i32;
    fn listen(fd: i32, backlog: i32) -> i32;
    fn accept(fd: i32, addr: *mut SockaddrUn, addrlen: *mut u32) -> i32;
    fn close(fd: i32) -> i32;
    fn unlink(path: *const u8) -> i32;
    fn mkdir(path: *const u8, mode: u32) -> i32;
    fn fcntl(fd: i32, cmd: i32, arg: i32) -> i32;
    fn send(fd: i32, buf: *const u8, len: usize, flags: i32) -> isize;
    fn recv(fd: i32, buf: *mut u8, len: usize, flags: i32) -> isize;
    fn __errno_location() -> *mut i32;
}

const AF_UNIX: i32 = 1;
const SOCK_STREAM: i32 = 1;
const O_NONBLOCK: i32 = 0o4000;
const F_SETFL: i32 = 4;
const EAGAIN: i32 = 11;

#[repr(C)]
struct SockaddrUn {
    sun_family: u16,
    sun_path: [u8; 108],
}

// ── X11 protocol constants ────────────────────────────────────────────────────
const ROOT_WIN: u32 = 0x0000_0001;
const ROOT_COLORMAP: u32 = 0x0000_0002;
const ROOT_VISUAL: u32 = 0x0000_0003;
const FIRST_USER_ATOM: u32 = 69;

// Request opcodes
const OP_CREATE_WINDOW: u8 = 1;
const OP_CHANGE_WINDOW_ATTRS: u8 = 2;
const OP_GET_WINDOW_ATTRS: u8 = 3;
const OP_DESTROY_WINDOW: u8 = 4;
const OP_MAP_WINDOW: u8 = 8;
const OP_MAP_SUBWINDOWS: u8 = 9;
const OP_UNMAP_WINDOW: u8 = 10;
const OP_CONFIGURE_WINDOW: u8 = 12;
const OP_GET_GEOMETRY: u8 = 14;
const OP_QUERY_TREE: u8 = 15;
const OP_INTERN_ATOM: u8 = 16;
const OP_GET_ATOM_NAME: u8 = 17;
const OP_CHANGE_PROPERTY: u8 = 18;
const OP_DELETE_PROPERTY: u8 = 19;
const OP_GET_PROPERTY: u8 = 20;
const OP_SEND_EVENT: u8 = 25;
const OP_GRAB_POINTER: u8 = 26;
const OP_UNGRAB_POINTER: u8 = 27;
const OP_GRAB_BUTTON: u8 = 28;
const OP_UNGRAB_BUTTON: u8 = 29;
const OP_GRAB_KEYBOARD: u8 = 31;
const OP_UNGRAB_KEYBOARD: u8 = 32;
const OP_GRAB_KEY: u8 = 33;
const OP_UNGRAB_KEY: u8 = 34;
const OP_ALLOW_EVENTS: u8 = 35;
const OP_QUERY_POINTER: u8 = 38;
const OP_WARP_POINTER: u8 = 41;
const OP_SET_INPUT_FOCUS: u8 = 42;
const OP_GET_INPUT_FOCUS: u8 = 43;
const OP_QUERY_KEYMAP: u8 = 44;
const OP_OPEN_FONT: u8 = 45;
const OP_CLOSE_FONT: u8 = 46;
const OP_CREATE_PIXMAP: u8 = 53;
const OP_FREE_PIXMAP: u8 = 54;
const OP_CREATE_GC: u8 = 55;
const OP_CHANGE_GC: u8 = 56;
const OP_COPY_GC: u8 = 57;
const OP_FREE_GC: u8 = 60;
const OP_CLEAR_AREA: u8 = 61;
const OP_COPY_AREA: u8 = 62;
const OP_POLY_FILL_RECT: u8 = 70;
const OP_PUT_IMAGE: u8 = 72;
const OP_GET_IMAGE: u8 = 73;
const OP_CREATE_COLORMAP: u8 = 78;
const OP_FREE_COLORMAP: u8 = 79;
const OP_ALLOC_COLOR: u8 = 84;
const OP_QUERY_EXTENSION: u8 = 98;
const OP_LIST_EXTENSIONS: u8 = 99;
const OP_CHANGE_KEYBOARD_MAPPING: u8 = 100;
const OP_GET_KEYBOARD_MAPPING: u8 = 101;
const OP_CHANGE_KEYBOARD_CONTROL: u8 = 102;
const OP_GET_KEYBOARD_CONTROL: u8 = 103;
const OP_BELL: u8 = 104;
const OP_CHANGE_POINTER_CONTROL: u8 = 105;
const OP_GET_SCREEN_SAVER: u8 = 108;
const OP_LIST_HOSTS: u8 = 110;
const OP_SET_ACCESS_CONTROL: u8 = 111;
const OP_SET_CLOSE_DOWN_MODE: u8 = 112;
const OP_KILL_CLIENT: u8 = 113;
const OP_GET_MODIFIER_MAPPING: u8 = 119;
const OP_NO_OPERATION: u8 = 127;

// Event codes
const EV_KEY_PRESS: u8 = 2;
const EV_KEY_RELEASE: u8 = 3;
const EV_BUTTON_PRESS: u8 = 4;
const EV_BUTTON_RELEASE: u8 = 5;
const EV_MOTION_NOTIFY: u8 = 6;
const EV_FOCUS_IN: u8 = 9;
const EV_FOCUS_OUT: u8 = 10;
const EV_EXPOSE: u8 = 12;
const EV_DESTROY_NOTIFY: u8 = 17;
const EV_UNMAP_NOTIFY: u8 = 18;
const EV_MAP_NOTIFY: u8 = 19;
const EV_CONFIGURE_NOTIFY: u8 = 22;

// ── Public types ──────────────────────────────────────────────────────────────

/// BGRA pixel buffer for an X11 window.
pub struct X11PixelBuffer {
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Actions emitted by the X11 server for the compositor to act on.
pub enum X11Action {
    MapWindow { window_id: u32, client_id: u32, x: i16, y: i16, width: u16, height: u16, title: [u8; 64] },
    UnmapWindow { window_id: u32 },
    DestroyWindow { window_id: u32 },
    ConfigureWindow { window_id: u32, x: Option<i16>, y: Option<i16>, width: Option<u16>, height: Option<u16> },
    TitleChanged { window_id: u32, title: [u8; 64] },
    FrameReady { window_id: u32, pixels: Vec<u8>, width: u32, height: u32 },
}

/// Tracked X11 window.
pub struct X11Window {
    pub xid: u32,
    pub client_id: u32,
    pub parent: u32,
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
    pub depth: u8,
    pub mapped: bool,
    pub event_mask: u32,
    pub background_pixel: u32,
    pub title: [u8; 64],
    pub shell_slot: Option<usize>,
}

/// A connected X11 client.
pub struct X11Client {
    pub fd: i32,
    pub client_id: u32,
    recv_buf: Vec<u8>,
    setup_done: bool,
    byte_order: u8,
    pub sequence: u16,
    resource_id_base: u32,
    resource_id_mask: u32,
    atoms: BTreeMap<String, u32>,
    next_atom: u32,
    gcs: BTreeMap<u32, u32>,
    pixmaps: BTreeMap<u32, (u16, u16, Vec<u8>)>,
    pub keyboard_focus: u32,
    event_queue: Vec<[u8; 32]>,
}

/// The X11 server, listening on /tmp/.X11-unix/X0.
pub struct X11Server {
    listener_fd: i32,
    pub clients: Vec<X11Client>,
    next_client_id: u32,
    pub windows: BTreeMap<u32, X11Window>,
    pub screen_width: u16,
    pub screen_height: u16,
}

// ── X11Client implementation ──────────────────────────────────────────────────

impl X11Client {
    pub fn new(fd: i32, client_id: u32) -> Self {
        let mut atoms: BTreeMap<String, u32> = BTreeMap::new();
        // Predefined atoms 1–68
        atoms.insert("PRIMARY".into(), 1);
        atoms.insert("SECONDARY".into(), 2);
        atoms.insert("ARC".into(), 3);
        atoms.insert("ATOM".into(), 4);
        atoms.insert("BITMAP".into(), 5);
        atoms.insert("CARDINAL".into(), 6);
        atoms.insert("COLORMAP".into(), 7);
        atoms.insert("CURSOR".into(), 8);
        atoms.insert("CUT_BUFFER0".into(), 9);
        atoms.insert("CUT_BUFFER1".into(), 10);
        atoms.insert("CUT_BUFFER2".into(), 11);
        atoms.insert("CUT_BUFFER3".into(), 12);
        atoms.insert("CUT_BUFFER4".into(), 13);
        atoms.insert("CUT_BUFFER5".into(), 14);
        atoms.insert("CUT_BUFFER6".into(), 15);
        atoms.insert("CUT_BUFFER7".into(), 16);
        atoms.insert("DRAWABLE".into(), 17);
        atoms.insert("FONT".into(), 18);
        atoms.insert("INTEGER".into(), 19);
        atoms.insert("PIXMAP".into(), 20);
        atoms.insert("POINT".into(), 21);
        atoms.insert("RECTANGLE".into(), 22);
        atoms.insert("RESOURCE_MANAGER".into(), 23);
        atoms.insert("RGB_COLOR_MAP".into(), 24);
        atoms.insert("RGB_BEST_MAP".into(), 25);
        atoms.insert("RGB_BLUE_MAP".into(), 26);
        atoms.insert("RGB_DEFAULT_MAP".into(), 27);
        atoms.insert("RGB_GRAY_MAP".into(), 28);
        atoms.insert("RGB_GREEN_MAP".into(), 29);
        atoms.insert("RGB_RED_MAP".into(), 30);
        atoms.insert("STRING".into(), 31);
        atoms.insert("VISUALID".into(), 32);
        atoms.insert("WINDOW".into(), 33);
        atoms.insert("WM_COMMAND".into(), 34);
        atoms.insert("WM_HINTS".into(), 35);
        atoms.insert("WM_CLIENT_MACHINE".into(), 36);
        atoms.insert("WM_ICON_NAME".into(), 37);
        atoms.insert("WM_ICON_SIZE".into(), 38);
        atoms.insert("WM_NAME".into(), 39);
        atoms.insert("WM_NORMAL_HINTS".into(), 40);
        atoms.insert("WM_SIZE_HINTS".into(), 41);
        atoms.insert("WM_ZOOM_HINTS".into(), 42);
        atoms.insert("MIN_SPACE".into(), 43);
        atoms.insert("NORM_SPACE".into(), 44);
        atoms.insert("MAX_SPACE".into(), 45);
        atoms.insert("END_SPACE".into(), 46);
        atoms.insert("SUPERSCRIPT_X".into(), 47);
        atoms.insert("SUPERSCRIPT_Y".into(), 48);
        atoms.insert("SUBSCRIPT_X".into(), 49);
        atoms.insert("SUBSCRIPT_Y".into(), 50);
        atoms.insert("UNDERLINE_POSITION".into(), 51);
        atoms.insert("UNDERLINE_THICKNESS".into(), 52);
        atoms.insert("STRIKEOUT_ASCENT".into(), 53);
        atoms.insert("STRIKEOUT_DESCENT".into(), 54);
        atoms.insert("ITALIC_ANGLE".into(), 55);
        atoms.insert("X_HEIGHT".into(), 56);
        atoms.insert("QUAD_WIDTH".into(), 57);
        atoms.insert("WEIGHT".into(), 58);
        atoms.insert("POINT_SIZE".into(), 59);
        atoms.insert("RESOLUTION".into(), 60);
        atoms.insert("COPYRIGHT".into(), 61);
        atoms.insert("NOTICE".into(), 62);
        atoms.insert("FONT_NAME".into(), 63);
        atoms.insert("FAMILY_NAME".into(), 64);
        atoms.insert("FULL_NAME".into(), 65);
        atoms.insert("CAP_HEIGHT".into(), 66);
        atoms.insert("WM_CLASS".into(), 67);
        atoms.insert("WM_TRANSIENT_FOR".into(), 68);

        Self {
            fd,
            client_id,
            recv_buf: Vec::new(),
            setup_done: false,
            byte_order: b'l',
            sequence: 0,
            resource_id_base: 0x00200000,
            resource_id_mask: 0x001fffff,
            atoms,
            next_atom: FIRST_USER_ATOM,
            gcs: BTreeMap::new(),
            pixmaps: BTreeMap::new(),
            keyboard_focus: ROOT_WIN,
            event_queue: Vec::new(),
        }
    }

    fn intern_atom(&mut self, name: &str, only_if_exists: bool) -> u32 {
        if let Some(&id) = self.atoms.get(name) {
            return id;
        }
        if only_if_exists {
            return 0;
        }
        let id = self.next_atom;
        self.next_atom += 1;
        self.atoms.insert(name.to_string(), id);
        id
    }

    fn atom_name(&self, id: u32) -> Option<&str> {
        for (name, &aid) in &self.atoms {
            if aid == id {
                return Some(name.as_str());
            }
        }
        None
    }

    pub fn process_requests(
        &mut self,
        data: &[u8],
        windows: &mut BTreeMap<u32, X11Window>,
        screen_w: u16,
        screen_h: u16,
    ) -> Vec<X11Action> {
        let mut actions = Vec::new();
        self.recv_buf.extend_from_slice(data);

        if !self.setup_done {
            // Need at least 12 bytes for setup
            if self.recv_buf.len() < 12 { return actions; }
            let order = self.recv_buf[0];
            self.byte_order = order;
            // Parse setup request length to know how many bytes to skip
            let name_len = if order == b'l' {
                u16::from_le_bytes([self.recv_buf[6], self.recv_buf[7]]) as usize
            } else {
                u16::from_be_bytes([self.recv_buf[6], self.recv_buf[7]]) as usize
            };
            let data_len = if order == b'l' {
                u16::from_le_bytes([self.recv_buf[8], self.recv_buf[9]]) as usize  
            } else {
                u16::from_be_bytes([self.recv_buf[8], self.recv_buf[9]]) as usize
            };
            // name is padded to 4-byte boundary, then data
            let name_pad = (4 - (name_len % 4)) % 4;
            let setup_len = 12 + name_len + name_pad + data_len;
            if self.recv_buf.len() < setup_len { return actions; }

            self.send_setup_reply(screen_w, screen_h);
            self.setup_done = true;
            self.recv_buf.drain(..setup_len);
        }

        // Process requests
        loop {
            if self.recv_buf.len() < 4 { break; }
            let opcode = self.recv_buf[0];
            let detail = self.recv_buf[1];
            let req_len_units = u16::from_le_bytes([self.recv_buf[2], self.recv_buf[3]]) as usize;
            let req_len = req_len_units * 4;
            if req_len < 4 { break; }
            if self.recv_buf.len() < req_len { break; }

            let req_data: Vec<u8> = self.recv_buf[4..req_len].to_vec();
            let seq = self.sequence;
            self.sequence = self.sequence.wrapping_add(1);

            self.dispatch_request(opcode, detail, &req_data, seq, windows, screen_w, screen_h, &mut actions);
            self.recv_buf.drain(..req_len);
        }

        actions
    }

    fn send_setup_reply(&mut self, screen_w: u16, screen_h: u16) {
        // Build the connection setup reply
        // Fixed part: 32 bytes
        // Vendor: "Eclipse\0" = 8 bytes  
        // 2 pixmap formats: 2 * 8 = 16 bytes
        // Screen: 40 + 8 (depth header) + 24 (visual) = 72 bytes
        // Total additional = 32 + 8 + 16 + 72 = 128 bytes = 32 four-byte units
        let additional_len: u16 = 32;

        let mut reply = Vec::with_capacity(8 + 128);
        // 8-byte header
        reply.push(1u8);  // success
        reply.push(0u8);  // pad
        reply.extend_from_slice(&11u16.to_le_bytes()); // protocol-major
        reply.extend_from_slice(&0u16.to_le_bytes());  // protocol-minor
        reply.extend_from_slice(&additional_len.to_le_bytes()); // additional-data-length

        // Additional data (32 bytes fixed)
        reply.extend_from_slice(&1u32.to_le_bytes());           // release-number
        reply.extend_from_slice(&0x00200000u32.to_le_bytes());  // resource-id-base
        reply.extend_from_slice(&0x001fffffu32.to_le_bytes());  // resource-id-mask
        reply.extend_from_slice(&256u32.to_le_bytes());         // motion-buffer-size
        reply.extend_from_slice(&8u16.to_le_bytes());           // vendor-length
        reply.extend_from_slice(&65535u16.to_le_bytes());       // max-request-length
        reply.push(1u8);  // num-screens
        reply.push(2u8);  // num-pixmap-formats
        reply.push(0u8);  // image-byte-order: LSBFirst
        reply.push(0u8);  // bitmap-format-bit-order: LSBFirst
        reply.push(32u8); // bitmap-scan-line-unit
        reply.push(32u8); // bitmap-scan-line-pad
        reply.push(8u8);  // min-keycode
        reply.push(255u8);// max-keycode
        reply.extend_from_slice(&[0u8; 4]); // pad

        // Vendor: "Eclipse\0" (8 bytes)
        reply.extend_from_slice(b"Eclipse\0");

        // Pixmap format 1: depth=8, bpp=8, scanpad=8, pad(5)
        reply.push(8u8);  // depth
        reply.push(8u8);  // bpp
        reply.push(8u8);  // scan-line-pad
        reply.extend_from_slice(&[0u8; 5]); // pad

        // Pixmap format 2: depth=32, bpp=32, scanpad=32, pad(5)
        reply.push(32u8); // depth
        reply.push(32u8); // bpp
        reply.push(32u8); // scan-line-pad
        reply.extend_from_slice(&[0u8; 5]); // pad

        // Screen (40 bytes header)
        reply.extend_from_slice(&ROOT_WIN.to_le_bytes());         // root window
        reply.extend_from_slice(&ROOT_COLORMAP.to_le_bytes());    // default-colormap
        reply.extend_from_slice(&0x00ffffffu32.to_le_bytes());    // white-pixel
        reply.extend_from_slice(&0x00000000u32.to_le_bytes());    // black-pixel
        reply.extend_from_slice(&0u32.to_le_bytes());             // current-input-masks
        reply.extend_from_slice(&screen_w.to_le_bytes());         // width-in-pixels
        reply.extend_from_slice(&screen_h.to_le_bytes());         // height-in-pixels
        reply.extend_from_slice(&508u16.to_le_bytes());           // width-in-mm
        reply.extend_from_slice(&285u16.to_le_bytes());           // height-in-mm
        reply.extend_from_slice(&1u16.to_le_bytes());             // min-installed-maps
        reply.extend_from_slice(&1u16.to_le_bytes());             // max-installed-maps
        reply.extend_from_slice(&ROOT_VISUAL.to_le_bytes());      // root-visual
        reply.push(0u8);  // backing-stores: Never
        reply.push(0u8);  // save-unders
        reply.push(24u8); // root-depth
        reply.push(1u8);  // num-depths

        // Depth info (8 bytes header)
        reply.push(24u8); // depth
        reply.push(0u8);  // pad
        reply.extend_from_slice(&1u16.to_le_bytes()); // num-visuals
        reply.extend_from_slice(&[0u8; 4]); // pad

        // Visual (24 bytes)
        reply.extend_from_slice(&ROOT_VISUAL.to_le_bytes()); // visual-id
        reply.push(4u8);  // class: TrueColor
        reply.push(8u8);  // bits-per-rgb
        reply.extend_from_slice(&256u16.to_le_bytes());       // colormap-entries
        reply.extend_from_slice(&0x00ff0000u32.to_le_bytes()); // red-mask
        reply.extend_from_slice(&0x0000ff00u32.to_le_bytes()); // green-mask
        reply.extend_from_slice(&0x000000ffu32.to_le_bytes()); // blue-mask
        reply.extend_from_slice(&[0u8; 4]); // pad

        send_x11_bytes(self.fd, &reply);
    }

    fn dispatch_request(
        &mut self,
        opcode: u8,
        detail: u8,
        data: &[u8],
        seq: u16,
        windows: &mut BTreeMap<u32, X11Window>,
        screen_w: u16,
        screen_h: u16,
        actions: &mut Vec<X11Action>,
    ) {
        let seq_le = seq.to_le_bytes();
        match opcode {
            OP_CREATE_WINDOW => {
                if data.len() < 28 { return; }
                let wid = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
                let parent = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
                let x = i16::from_le_bytes([data[8], data[9]]);
                let y = i16::from_le_bytes([data[10], data[11]]);
                let width = u16::from_le_bytes([data[12], data[13]]);
                let height = u16::from_le_bytes([data[14], data[15]]);
                // [16..18] border-width, [18..20] class, [20..24] visual
                let value_mask = u32::from_le_bytes([data[24], data[25], data[26], data[27]]);

                let mut event_mask: u32 = 0;
                let mut background_pixel: u32 = 0;
                let mut attr_offset = 28usize;
                for bit in 0u32..16 {
                    if value_mask & (1 << bit) != 0 {
                        if attr_offset + 4 > data.len() { break; }
                        let val = u32::from_le_bytes([data[attr_offset], data[attr_offset+1], data[attr_offset+2], data[attr_offset+3]]);
                        match bit {
                            1 => background_pixel = val,  // CW_BACK_PIXEL
                            11 => event_mask = val,        // CW_EVENT_MASK
                            _ => {}
                        }
                        attr_offset += 4;
                    }
                }

                let win = X11Window {
                    xid: wid,
                    client_id: self.client_id,
                    parent,
                    x, y, width, height,
                    depth: detail,
                    mapped: false,
                    event_mask,
                    background_pixel,
                    title: [0u8; 64],
                    shell_slot: None,
                };
                windows.insert(wid, win);
            }

            OP_CHANGE_WINDOW_ATTRS => {
                if data.len() < 8 { return; }
                let wid = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
                let value_mask = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
                let mut attr_offset = 8usize;
                if let Some(win) = windows.get_mut(&wid) {
                    for bit in 0u32..16 {
                        if value_mask & (1 << bit) != 0 {
                            if attr_offset + 4 > data.len() { break; }
                            let val = u32::from_le_bytes([data[attr_offset], data[attr_offset+1], data[attr_offset+2], data[attr_offset+3]]);
                            match bit {
                                1 => win.background_pixel = val,
                                11 => win.event_mask = val,
                                _ => {}
                            }
                            attr_offset += 4;
                        }
                    }
                }
            }

            OP_GET_WINDOW_ATTRS => {
                if data.len() < 4 { return; }
                let wid = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
                let (visual, class, map_state) = if let Some(win) = windows.get(&wid) {
                    (ROOT_VISUAL, 1u16, if win.mapped { 2u8 } else { 0u8 })
                } else {
                    (ROOT_VISUAL, 1u16, 0u8)
                };
                let mut reply = [0u8; 44];
                reply[0] = 1; // reply
                reply[1] = 0; // backing-store: Never
                reply[2..4].copy_from_slice(&seq_le);
                reply[4..8].copy_from_slice(&3u32.to_le_bytes()); // reply-length = 3
                reply[8..12].copy_from_slice(&visual.to_le_bytes());
                reply[12..14].copy_from_slice(&class.to_le_bytes());
                reply[14] = 1; // bit-gravity: NorthWest
                reply[15] = 1; // win-gravity: NorthWest
                // backing-planes, backing-pixel: 0
                reply[24] = 0; // save-under
                reply[25] = 0; // map-is-installed
                reply[26] = map_state;
                reply[27] = 0; // override-redirect
                reply[28..32].copy_from_slice(&ROOT_COLORMAP.to_le_bytes()); // colormap
                send_x11_bytes(self.fd, &reply);
            }

            OP_DESTROY_WINDOW => {
                if data.len() < 4 { return; }
                let wid = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
                windows.remove(&wid);
                actions.push(X11Action::DestroyWindow { window_id: wid });
            }

            OP_MAP_WINDOW => {
                if data.len() < 4 { return; }
                let wid = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
                if let Some(win) = windows.get_mut(&wid) {
                    win.mapped = true;
                    let (x, y, w, h, title) = (win.x, win.y, win.width, win.height, win.title);
                    let cid = win.client_id;
                    // MapNotify event
                    let map_ev = build_map_notify(seq, wid, wid, false);
                    self.event_queue.push(map_ev);
                    // Expose event
                    let exp_ev = build_expose(seq, wid, 0, 0, w, h, 0);
                    self.event_queue.push(exp_ev);
                    actions.push(X11Action::MapWindow { window_id: wid, client_id: cid, x, y, width: w, height: h, title });
                }
            }

            OP_MAP_SUBWINDOWS => {
                if data.len() < 4 { return; }
                let parent = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
                let children: Vec<u32> = windows.keys()
                    .copied()
                    .filter(|&k| windows[&k].parent == parent)
                    .collect();
                for child_id in children {
                    if let Some(win) = windows.get_mut(&child_id) {
                        win.mapped = true;
                        let (x, y, w, h, title, cid) = (win.x, win.y, win.width, win.height, win.title, win.client_id);
                        let map_ev = build_map_notify(seq, child_id, child_id, false);
                        self.event_queue.push(map_ev);
                        actions.push(X11Action::MapWindow { window_id: child_id, client_id: cid, x, y, width: w, height: h, title });
                    }
                }
            }

            OP_UNMAP_WINDOW => {
                if data.len() < 4 { return; }
                let wid = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
                if let Some(win) = windows.get_mut(&wid) {
                    win.mapped = false;
                    let mut ev = [0u8; 32];
                    ev[0] = EV_UNMAP_NOTIFY;
                    ev[2..4].copy_from_slice(&seq_le);
                    ev[4..8].copy_from_slice(&wid.to_le_bytes());
                    ev[8..12].copy_from_slice(&wid.to_le_bytes());
                    self.event_queue.push(ev);
                }
                actions.push(X11Action::UnmapWindow { window_id: wid });
            }

            OP_CONFIGURE_WINDOW => {
                if data.len() < 8 { return; }
                let wid = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
                let value_mask = u16::from_le_bytes([data[4], data[5]]);
                let mut offset = 8usize;

                let mut new_x: Option<i16> = None;
                let mut new_y: Option<i16> = None;
                let mut new_w: Option<u16> = None;
                let mut new_h: Option<u16> = None;

                macro_rules! read_u32 {
                    () => {{
                        if offset + 4 > data.len() { 0u32 } else {
                            let v = u32::from_le_bytes([data[offset], data[offset+1], data[offset+2], data[offset+3]]);
                            offset += 4;
                            v
                        }
                    }};
                }

                for bit in 0u16..7 {
                    if value_mask & (1 << bit) != 0 {
                        let val = read_u32!();
                        match bit {
                            0 => new_x = Some(val as i16),
                            1 => new_y = Some(val as i16),
                            2 => new_w = Some(val as u16),
                            3 => new_h = Some(val as u16),
                            _ => {}
                        }
                    }
                }

                if let Some(win) = windows.get_mut(&wid) {
                    if let Some(v) = new_x { win.x = v; }
                    if let Some(v) = new_y { win.y = v; }
                    if let Some(v) = new_w { win.width = v; }
                    if let Some(v) = new_h { win.height = v; }
                    let (wx, wy, ww, wh) = (win.x, win.y, win.width, win.height);
                    // ConfigureNotify
                    let mut ev = [0u8; 32];
                    ev[0] = EV_CONFIGURE_NOTIFY;
                    ev[2..4].copy_from_slice(&seq_le);
                    ev[4..8].copy_from_slice(&wid.to_le_bytes());
                    ev[8..12].copy_from_slice(&wid.to_le_bytes());
                    ev[12..16].copy_from_slice(&0u32.to_le_bytes()); // above-sibling
                    ev[16..18].copy_from_slice(&wx.to_le_bytes());
                    ev[18..20].copy_from_slice(&wy.to_le_bytes());
                    ev[20..22].copy_from_slice(&ww.to_le_bytes());
                    ev[22..24].copy_from_slice(&wh.to_le_bytes());
                    self.event_queue.push(ev);
                    actions.push(X11Action::ConfigureWindow { window_id: wid, x: new_x, y: new_y, width: new_w, height: new_h });
                }
            }

            OP_GET_GEOMETRY => {
                if data.len() < 4 { return; }
                let drawable = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
                let (depth, x, y, w, h) = if let Some(win) = windows.get(&drawable) {
                    (win.depth, win.x, win.y, win.width, win.height)
                } else if let Some((pw, ph, _)) = self.pixmaps.get(&drawable) {
                    (32u8, 0i16, 0i16, *pw, *ph)
                } else {
                    (24u8, 0i16, 0i16, screen_w, screen_h)
                };
                let mut reply = [0u8; 32];
                reply[0] = 1;
                reply[1] = depth;
                reply[2..4].copy_from_slice(&seq_le);
                reply[4..8].copy_from_slice(&0u32.to_le_bytes());
                reply[8..12].copy_from_slice(&ROOT_WIN.to_le_bytes());
                reply[12..14].copy_from_slice(&x.to_le_bytes());
                reply[14..16].copy_from_slice(&y.to_le_bytes());
                reply[16..18].copy_from_slice(&w.to_le_bytes());
                reply[18..20].copy_from_slice(&h.to_le_bytes());
                send_x11_bytes(self.fd, &reply);
            }

            OP_QUERY_TREE => {
                if data.len() < 4 { return; }
                // Return empty children list
                let mut reply = [0u8; 32];
                reply[0] = 1;
                reply[2..4].copy_from_slice(&seq_le);
                reply[4..8].copy_from_slice(&0u32.to_le_bytes());
                reply[8..12].copy_from_slice(&ROOT_WIN.to_le_bytes());
                reply[12..16].copy_from_slice(&ROOT_WIN.to_le_bytes());
                reply[16..18].copy_from_slice(&0u16.to_le_bytes()); // num-children
                send_x11_bytes(self.fd, &reply);
            }

            OP_INTERN_ATOM => {
                // detail byte = only_if_exists
                let only_if_exists = detail != 0;
                if data.len() < 4 { return; }
                let name_len = u16::from_le_bytes([data[0], data[1]]) as usize;
                // name starts at offset 4 (after 2-byte name_len + 2-byte pad)
                let name_start = 4;
                if data.len() < name_start + name_len { return; }
                let name = core::str::from_utf8(&data[name_start..name_start + name_len])
                    .unwrap_or("")
                    .to_string();
                let atom_id = self.intern_atom(&name, only_if_exists);
                let mut reply = [0u8; 32];
                reply[0] = 1;
                reply[2..4].copy_from_slice(&seq_le);
                reply[8..12].copy_from_slice(&atom_id.to_le_bytes());
                send_x11_bytes(self.fd, &reply);
            }

            OP_GET_ATOM_NAME => {
                if data.len() < 4 { return; }
                let atom_id = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
                let name = self.atom_name(atom_id).unwrap_or("UNKNOWN").to_string();
                let name_bytes = name.as_bytes();
                let name_len = name_bytes.len();
                let name_pad = (4 - (name_len % 4)) % 4;
                let extra_len = (name_len + name_pad + 3) / 4;
                let mut reply = Vec::with_capacity(32 + name_len + name_pad);
                reply.resize(32, 0u8);
                reply[0] = 1;
                reply[2..4].copy_from_slice(&seq_le);
                reply[4..8].copy_from_slice(&(extra_len as u32).to_le_bytes());
                reply[8..10].copy_from_slice(&(name_len as u16).to_le_bytes());
                reply.extend_from_slice(name_bytes);
                reply.resize(32 + name_len + name_pad, 0u8);
                send_x11_bytes(self.fd, &reply);
            }

            OP_CHANGE_PROPERTY => {
                // detail = mode (Replace/Prepend/Append)
                if data.len() < 16 { return; }
                let wid = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
                let property = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
                let prop_type = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
                let format = data[12]; // 8, 16, or 32
                let data_len = u32::from_le_bytes([data[16], data[17], data[18], data[19]]) as usize;
                let bytes_per_item = (format as usize) / 8;
                let total_bytes = data_len * bytes_per_item;
                let prop_data_start = 20;

                // WM_NAME atom = 39, STRING = 31
                let wm_name_atom = 39u32;
                let string_atom = 31u32;
                let utf8_string_atom = self.atoms.get("UTF8_STRING").copied().unwrap_or(0);
                let net_wm_name_atom = self.atoms.get("_NET_WM_NAME").copied().unwrap_or(0);

                let is_title = property == wm_name_atom || (net_wm_name_atom != 0 && property == net_wm_name_atom);
                let is_string = prop_type == string_atom || (utf8_string_atom != 0 && prop_type == utf8_string_atom);

                if is_title && is_string {
                    if let Some(win) = windows.get_mut(&wid) {
                        let copy_len = total_bytes.min(63);
                        if prop_data_start + copy_len <= data.len() {
                            let old_title = win.title;
                            win.title = [0u8; 64];
                            win.title[..copy_len].copy_from_slice(&data[prop_data_start..prop_data_start + copy_len]);
                            if win.title != old_title {
                                actions.push(X11Action::TitleChanged { window_id: wid, title: win.title });
                            }
                        }
                    }
                }
                // No reply
            }

            OP_DELETE_PROPERTY => {
                // No-op, no reply
            }

            OP_GET_PROPERTY => {
                // Return empty reply
                let mut reply = [0u8; 32];
                reply[0] = 1;
                reply[2..4].copy_from_slice(&seq_le);
                // type=0, bytes-after=0, value-len=0
                send_x11_bytes(self.fd, &reply);
            }

            OP_SEND_EVENT => {
                // Consume but do nothing special
            }

            OP_GRAB_POINTER => {
                // Reply with GrabSuccess
                let mut reply = [0u8; 32];
                reply[0] = 1;
                reply[1] = 0; // GrabSuccess
                reply[2..4].copy_from_slice(&seq_le);
                send_x11_bytes(self.fd, &reply);
            }

            OP_UNGRAB_POINTER | OP_GRAB_BUTTON | OP_UNGRAB_BUTTON |
            OP_UNGRAB_KEYBOARD | OP_GRAB_KEY | OP_UNGRAB_KEY | OP_ALLOW_EVENTS |
            OP_WARP_POINTER => {
                // No reply
            }

            OP_GRAB_KEYBOARD => {
                let mut reply = [0u8; 32];
                reply[0] = 1;
                reply[1] = 0; // GrabSuccess
                reply[2..4].copy_from_slice(&seq_le);
                send_x11_bytes(self.fd, &reply);
            }

            OP_QUERY_POINTER => {
                let mut reply = [0u8; 32];
                reply[0] = 1;
                reply[1] = 1; // same-screen
                reply[2..4].copy_from_slice(&seq_le);
                reply[8..12].copy_from_slice(&ROOT_WIN.to_le_bytes());
                send_x11_bytes(self.fd, &reply);
            }

            OP_SET_INPUT_FOCUS => {
                if data.len() >= 4 {
                    let focus_win = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
                    self.keyboard_focus = focus_win;
                }
            }

            OP_GET_INPUT_FOCUS => {
                let mut reply = [0u8; 32];
                reply[0] = 1;
                reply[1] = 1; // revert-to: PointerRoot
                reply[2..4].copy_from_slice(&seq_le);
                reply[8..12].copy_from_slice(&self.keyboard_focus.to_le_bytes());
                send_x11_bytes(self.fd, &reply);
            }

            OP_QUERY_KEYMAP => {
                let mut reply = [0u8; 40];
                reply[0] = 1;
                reply[2..4].copy_from_slice(&seq_le);
                reply[4..8].copy_from_slice(&2u32.to_le_bytes()); // 8 extra bytes
                send_x11_bytes(self.fd, &reply);
            }

            OP_OPEN_FONT | OP_CLOSE_FONT => {
                // No reply
            }

            OP_CREATE_PIXMAP => {
                if data.len() < 12 { return; }
                let pixmap_id = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
                // drawable at [4..8]
                let width = u16::from_le_bytes([data[8], data[9]]);
                let height = u16::from_le_bytes([data[10], data[11]]);
                let pixels = vec![0u8; (width as usize) * (height as usize) * 4];
                self.pixmaps.insert(pixmap_id, (width, height, pixels));
            }

            OP_FREE_PIXMAP => {
                if data.len() >= 4 {
                    let pid = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
                    self.pixmaps.remove(&pid);
                }
            }

            OP_CREATE_GC => {
                if data.len() < 8 { return; }
                let gc_id = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
                let drawable = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
                self.gcs.insert(gc_id, drawable);
            }

            OP_CHANGE_GC | OP_COPY_GC => {
                // No-op, no reply
            }

            OP_FREE_GC => {
                if data.len() >= 4 {
                    let gc_id = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
                    self.gcs.remove(&gc_id);
                }
            }

            OP_CLEAR_AREA | OP_COPY_AREA | OP_POLY_FILL_RECT => {
                // No reply
            }

            OP_PUT_IMAGE => {
                // format = detail (0=XYBitmap, 1=XYPixmap, 2=ZPixmap)
                let format = detail;
                if data.len() < 20 { return; }
                let drawable = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
                // gc at [4..8]
                let width = u16::from_le_bytes([data[8], data[9]]) as u32;
                let height = u16::from_le_bytes([data[10], data[11]]) as u32;
                let dst_x = i16::from_le_bytes([data[12], data[13]]);
                let dst_y = i16::from_le_bytes([data[14], data[15]]);
                let img_depth = data[17];
                let img_data = &data[20..];

                if format == 2 {
                    // ZPixmap
                    let pixels: Vec<u8> = if img_depth == 32 || img_depth == 24 {
                        if img_depth == 32 {
                            img_data[..img_data.len().min((width * height * 4) as usize)].to_vec()
                        } else {
                            // 24bpp -> 32bpp conversion
                            let row_stride = ((width * 3 + 3) & !3) as usize;
                            let mut out = vec![0u8; (width * height * 4) as usize];
                            for row in 0..height as usize {
                                let src_row = row * row_stride;
                                let dst_row = row * width as usize * 4;
                                for col in 0..width as usize {
                                    let s = src_row + col * 3;
                                    let d = dst_row + col * 4;
                                    if s + 2 < img_data.len() && d + 3 < out.len() {
                                        out[d] = img_data[s];
                                        out[d+1] = img_data[s+1];
                                        out[d+2] = img_data[s+2];
                                        out[d+3] = 0xff;
                                    }
                                }
                            }
                            out
                        }
                    } else {
                        return;
                    };

                    // Is it a window?
                    if windows.contains_key(&drawable) {
                        if let Some(win) = windows.get(&drawable) {
                            let win_w = win.width as u32;
                            let win_h = win.height as u32;
                            let wid = win.xid;
                            drop(win);

                            if dst_x == 0 && dst_y == 0 && width == win_w && height == win_h {
                                actions.push(X11Action::FrameReady { window_id: wid, pixels, width, height });
                            } else {
                                // Sub-region update — emit as FrameReady for the sub-region
                                actions.push(X11Action::FrameReady { window_id: wid, pixels, width, height });
                            }
                        }
                    } else if self.pixmaps.contains_key(&drawable) {
                        if let Some(entry) = self.pixmaps.get_mut(&drawable) {
                            let copy_len = pixels.len().min(entry.2.len());
                            entry.2[..copy_len].copy_from_slice(&pixels[..copy_len]);
                        }
                    }
                }
            }

            OP_GET_IMAGE => {
                // Send error (BadMatch = 8)
                let mut err = [0u8; 32];
                err[0] = 0; // error
                err[1] = 8; // BadMatch
                err[2..4].copy_from_slice(&seq_le);
                send_x11_bytes(self.fd, &err);
            }

            OP_CREATE_COLORMAP | OP_FREE_COLORMAP => {
                // No reply
            }

            OP_ALLOC_COLOR => {
                if data.len() < 8 { return; }
                let red = u16::from_le_bytes([data[4], data[5]]);
                let green = u16::from_le_bytes([data[6], data[7]]);
                let blue = if data.len() >= 10 { u16::from_le_bytes([data[8], data[9]]) } else { 0 };
                let pixel = ((red as u32 >> 8) << 16) | ((green as u32 >> 8) << 8) | (blue as u32 >> 8);
                let mut reply = [0u8; 32];
                reply[0] = 1;
                reply[2..4].copy_from_slice(&seq_le);
                reply[8..10].copy_from_slice(&red.to_le_bytes());
                reply[10..12].copy_from_slice(&green.to_le_bytes());
                reply[12..14].copy_from_slice(&blue.to_le_bytes());
                reply[16..20].copy_from_slice(&pixel.to_le_bytes());
                send_x11_bytes(self.fd, &reply);
            }

            OP_QUERY_EXTENSION => {
                if data.len() < 4 { return; }
                let name_len = u16::from_le_bytes([data[0], data[1]]) as usize;
                let _ = name_len; // not present for any extension
                let mut reply = [0u8; 32];
                reply[0] = 1;
                reply[2..4].copy_from_slice(&seq_le);
                // present=0, all zeros
                send_x11_bytes(self.fd, &reply);
            }

            OP_LIST_EXTENSIONS => {
                let mut reply = [0u8; 32];
                reply[0] = 1;
                reply[1] = 0; // num-strings = 0
                reply[2..4].copy_from_slice(&seq_le);
                send_x11_bytes(self.fd, &reply);
            }

            OP_CHANGE_KEYBOARD_MAPPING => {
                // No reply
            }

            OP_GET_KEYBOARD_MAPPING => {
                if data.len() < 2 { return; }
                let first_keycode = data[0];
                let count = data[1] as usize;
                let keysyms_per = 2u8;
                let reply_len = (count * keysyms_per as usize * 4) / 4; // in 4-byte units

                let mut reply = Vec::with_capacity(32 + count * 8);
                reply.resize(32, 0u8);
                reply[0] = 1;
                reply[1] = keysyms_per;
                reply[2..4].copy_from_slice(&seq_le);
                reply[4..8].copy_from_slice(&(reply_len as u32).to_le_bytes());

                // Keymap table: keycodes 8-255, each with [unshifted, shifted]
                static KEYMAP: &[[u32; 2]; 248] = &build_keymap();
                for i in 0..count {
                    let kc = first_keycode as usize + i;
                    let (ks0, ks1) = if kc >= 8 && kc <= 255 {
                        let e = KEYMAP[kc - 8];
                        (e[0], e[1])
                    } else {
                        (0, 0)
                    };
                    reply.extend_from_slice(&ks0.to_le_bytes());
                    reply.extend_from_slice(&ks1.to_le_bytes());
                }
                send_x11_bytes(self.fd, &reply);
            }

            OP_CHANGE_KEYBOARD_CONTROL | OP_BELL | OP_CHANGE_POINTER_CONTROL => {
                // No reply
            }

            OP_GET_KEYBOARD_CONTROL => {
                let mut reply = [0u8; 32];
                reply[0] = 1;
                reply[1] = 0; // global-auto-repeat: off
                reply[2..4].copy_from_slice(&seq_le);
                reply[4..8].copy_from_slice(&0u32.to_le_bytes());
                // led-mask, key-click-percent, bell-percent, bell-pitch, bell-duration: 0
                send_x11_bytes(self.fd, &reply);
            }

            OP_GET_SCREEN_SAVER => {
                let mut reply = [0u8; 32];
                reply[0] = 1;
                reply[2..4].copy_from_slice(&seq_le);
                send_x11_bytes(self.fd, &reply);
            }

            OP_LIST_HOSTS => {
                let mut reply = [0u8; 32];
                reply[0] = 1;
                reply[1] = 1; // mode: Enabled
                reply[2..4].copy_from_slice(&seq_le);
                reply[8..10].copy_from_slice(&0u16.to_le_bytes()); // num-hosts
                send_x11_bytes(self.fd, &reply);
            }

            OP_SET_ACCESS_CONTROL | OP_SET_CLOSE_DOWN_MODE | OP_KILL_CLIENT => {
                // No reply
            }

            OP_GET_MODIFIER_MAPPING => {
                let keycodes_per = 2u8;
                let data_size = 8 * keycodes_per as usize; // 16 bytes
                let reply_len = (data_size + 3) / 4; // 4
                let mut reply = Vec::with_capacity(32 + data_size);
                reply.resize(32, 0u8);
                reply[0] = 1;
                reply[1] = keycodes_per;
                reply[2..4].copy_from_slice(&seq_le);
                reply[4..8].copy_from_slice(&(reply_len as u32).to_le_bytes());
                // Modifier keycodes (8 modifiers * 2 keycodes)
                let modifier_keys: [u8; 16] = [
                    50, 62, // Shift
                    66, 0,  // Lock (CapsLock)
                    37, 109, // Control
                    64, 113, // Mod1 (Alt)
                    0, 0,   // Mod2
                    0, 0,   // Mod3
                    0, 0,   // Mod4 (Super)
                    0, 0,   // Mod5
                ];
                reply.extend_from_slice(&modifier_keys);
                send_x11_bytes(self.fd, &reply);
            }

            OP_NO_OPERATION => {
                // Just consume bytes
            }

            _ => {
                // Unknown opcode — send BadRequest error
                let mut err = [0u8; 32];
                err[0] = 0; // error
                err[1] = 1; // BadRequest
                err[2..4].copy_from_slice(&seq_le);
                err[10] = opcode;
                send_x11_bytes(self.fd, &err);
            }
        }
    }
}

// ── X11Server implementation ──────────────────────────────────────────────────

impl X11Server {
    pub fn new(screen_width: u16, screen_height: u16) -> Option<Self> {
        let dir = b"/tmp/.X11-unix\0";
        let path = b"/tmp/.X11-unix/X0\0";
        unsafe {
            mkdir(dir.as_ptr(), 0o1777);
            unlink(path.as_ptr());
            let fd = socket(AF_UNIX, SOCK_STREAM, 0);
            if fd < 0 { return None; }

            let mut addr = SockaddrUn { sun_family: AF_UNIX as u16, sun_path: [0u8; 108] };
            let path_bytes = b"/tmp/.X11-unix/X0";
            addr.sun_path[..path_bytes.len()].copy_from_slice(path_bytes);
            let addrlen = (2 + path_bytes.len() + 1) as u32;
            if bind(fd, &addr, addrlen) < 0 {
                close(fd);
                return None;
            }
            if listen(fd, 16) < 0 {
                close(fd);
                return None;
            }
            // Set nonblocking
            let flags = fcntl(fd, 3 /* F_GETFL */, 0);
            fcntl(fd, F_SETFL, flags | O_NONBLOCK);

            Some(Self {
                listener_fd: fd,
                clients: Vec::new(),
                next_client_id: 1,
                windows: BTreeMap::new(),
                screen_width,
                screen_height,
            })
        }
    }

    pub fn poll(&mut self, fb_w: u16, fb_h: u16) -> Vec<X11Action> {
        let mut actions = Vec::new();

        // Accept new connections
        loop {
            let mut dummy = SockaddrUn { sun_family: 0, sun_path: [0u8; 108] };
            let mut addrlen = core::mem::size_of::<SockaddrUn>() as u32;
            let client_fd = unsafe { accept(self.listener_fd, &mut dummy, &mut addrlen) };
            if client_fd < 0 { break; }
            // Set client socket nonblocking
            unsafe {
                let flags = fcntl(client_fd, 3 /* F_GETFL */, 0);
                fcntl(client_fd, F_SETFL, flags | O_NONBLOCK);
            }
            let cid = self.next_client_id;
            self.next_client_id += 1;
            self.clients.push(X11Client::new(client_fd, cid));
        }

        // Process data from each client
        let mut disconnected = Vec::new();
        let screen_w = self.screen_width;
        let screen_h = self.screen_height;
        let _ = (fb_w, fb_h);

        for i in 0..self.clients.len() {
            let mut buf = [0u8; 65536];
            let n = unsafe { recv(self.clients[i].fd, buf.as_mut_ptr(), buf.len(), 0) };
            let data: Vec<u8> = if n > 0 {
                buf[..n as usize].to_vec()
            } else if n == 0 {
                disconnected.push(i);
                continue;
            } else {
                let errno = unsafe { *__errno_location() };
                if errno == EAGAIN { vec![] } else { disconnected.push(i); continue; }
            };
            let client_actions = self.clients[i].process_requests(&data, &mut self.windows, screen_w, screen_h);
            actions.extend(client_actions);
        }

        // Remove disconnected clients (in reverse order to preserve indices)
        for &idx in disconnected.iter().rev() {
            let client = &self.clients[idx];
            let cid = client.client_id;
            // Emit destroy for all windows owned by this client
            let owned: Vec<u32> = self.windows.iter()
                .filter(|(_, w)| w.client_id == cid)
                .map(|(&k, _)| k)
                .collect();
            for wid in owned {
                self.windows.remove(&wid);
                actions.push(X11Action::DestroyWindow { window_id: wid });
            }
            unsafe { close(self.clients[idx].fd); }
            self.clients.remove(idx);
        }

        actions
    }

    pub fn send_key_event(&mut self, window_id: u32, keycode: u8, pressed: bool, time: u32, state: u16) {
        let client_id = self.windows.get(&window_id).map(|w| w.client_id);
        if let Some(cid) = client_id {
            if let Some(client) = self.clients.iter_mut().find(|c| c.client_id == cid) {
                let seq = client.sequence;
                let event_code = if pressed { EV_KEY_PRESS } else { EV_KEY_RELEASE };
                let ev = build_key_press_release(
                    event_code, time, ROOT_WIN, window_id, 0,
                    0, 0, 0, 0, state, keycode, true, seq,
                );
                client.event_queue.push(ev);
            }
        }
    }

    pub fn send_button_event(&mut self, window_id: u32, button: u8, pressed: bool, x: i16, y: i16, time: u32) {
        let client_id = self.windows.get(&window_id).map(|w| w.client_id);
        if let Some(cid) = client_id {
            if let Some(client) = self.clients.iter_mut().find(|c| c.client_id == cid) {
                let seq = client.sequence;
                let event_code = if pressed { EV_BUTTON_PRESS } else { EV_BUTTON_RELEASE };
                let mut ev = [0u8; 32];
                ev[0] = event_code;
                ev[1] = button;
                ev[2..4].copy_from_slice(&seq.to_le_bytes());
                ev[4..8].copy_from_slice(&time.to_le_bytes());
                ev[8..12].copy_from_slice(&ROOT_WIN.to_le_bytes());
                ev[12..16].copy_from_slice(&window_id.to_le_bytes());
                ev[20..22].copy_from_slice(&x.to_le_bytes());
                ev[22..24].copy_from_slice(&y.to_le_bytes());
                ev[26..28].copy_from_slice(&x.to_le_bytes());
                ev[28..30].copy_from_slice(&y.to_le_bytes());
                ev[30] = 1; // same-screen
                client.event_queue.push(ev);
            }
        }
    }

    pub fn send_motion_event(&mut self, window_id: u32, x: i16, y: i16, time: u32) {
        let client_id = self.windows.get(&window_id).map(|w| w.client_id);
        if let Some(cid) = client_id {
            if let Some(client) = self.clients.iter_mut().find(|c| c.client_id == cid) {
                let seq = client.sequence;
                let mut ev = [0u8; 32];
                ev[0] = EV_MOTION_NOTIFY;
                ev[1] = 0;
                ev[2..4].copy_from_slice(&seq.to_le_bytes());
                ev[4..8].copy_from_slice(&time.to_le_bytes());
                ev[8..12].copy_from_slice(&ROOT_WIN.to_le_bytes());
                ev[12..16].copy_from_slice(&window_id.to_le_bytes());
                ev[16..18].copy_from_slice(&x.to_le_bytes());
                ev[18..20].copy_from_slice(&y.to_le_bytes());
                ev[20..22].copy_from_slice(&x.to_le_bytes());
                ev[22..24].copy_from_slice(&y.to_le_bytes());
                ev[30] = 1; // same-screen
                client.event_queue.push(ev);
            }
        }
    }

    pub fn send_focus_event(&mut self, window_id: u32, in_focus: bool) {
        let client_id = self.windows.get(&window_id).map(|w| w.client_id);
        if let Some(cid) = client_id {
            if let Some(client) = self.clients.iter_mut().find(|c| c.client_id == cid) {
                let seq = client.sequence;
                let event_code = if in_focus { EV_FOCUS_IN } else { EV_FOCUS_OUT };
                let mut ev = [0u8; 32];
                ev[0] = event_code;
                ev[1] = 0; // NotifyNormal
                ev[2..4].copy_from_slice(&seq.to_le_bytes());
                ev[4..8].copy_from_slice(&window_id.to_le_bytes());
                client.event_queue.push(ev);
            }
        }
    }

    pub fn flush_events(&mut self) {
        for client in &mut self.clients {
            let events: Vec<[u8; 32]> = client.event_queue.drain(..).collect();
            for ev in events {
                send_x11_bytes(client.fd, &ev);
            }
        }
    }
}

impl Drop for X11Server {
    fn drop(&mut self) {
        unsafe {
            close(self.listener_fd);
            unlink(b"/tmp/.X11-unix/X0\0".as_ptr());
        }
    }
}

// ── Helper functions ──────────────────────────────────────────────────────────

fn send_x11_bytes(fd: i32, buf: &[u8]) {
    let mut sent = 0usize;
    while sent < buf.len() {
        let n = unsafe { send(fd, buf.as_ptr().add(sent), buf.len() - sent, 0) };
        if n <= 0 { break; }
        sent += n as usize;
    }
}

fn build_map_notify(sequence: u16, event_win: u32, win: u32, override_redirect: bool) -> [u8; 32] {
    let mut ev = [0u8; 32];
    ev[0] = EV_MAP_NOTIFY;
    ev[1] = 0;
    ev[2..4].copy_from_slice(&sequence.to_le_bytes());
    ev[4..8].copy_from_slice(&event_win.to_le_bytes());
    ev[8..12].copy_from_slice(&win.to_le_bytes());
    ev[12] = if override_redirect { 1 } else { 0 };
    ev
}

fn build_expose(sequence: u16, win: u32, x: u16, y: u16, w: u16, h: u16, count: u16) -> [u8; 32] {
    let mut ev = [0u8; 32];
    ev[0] = EV_EXPOSE;
    ev[2..4].copy_from_slice(&sequence.to_le_bytes());
    ev[4..8].copy_from_slice(&win.to_le_bytes());
    ev[8..10].copy_from_slice(&x.to_le_bytes());
    ev[10..12].copy_from_slice(&y.to_le_bytes());
    ev[12..14].copy_from_slice(&w.to_le_bytes());
    ev[14..16].copy_from_slice(&h.to_le_bytes());
    ev[16..18].copy_from_slice(&count.to_le_bytes());
    ev
}

fn build_key_press_release(
    event_code: u8, time: u32, root: u32, event: u32, child: u32,
    root_x: i16, root_y: i16, event_x: i16, event_y: i16,
    state: u16, keycode: u8, same_screen: bool, sequence: u16,
) -> [u8; 32] {
    let mut ev = [0u8; 32];
    ev[0] = event_code;
    ev[1] = keycode;
    ev[2..4].copy_from_slice(&sequence.to_le_bytes());
    ev[4..8].copy_from_slice(&time.to_le_bytes());
    ev[8..12].copy_from_slice(&root.to_le_bytes());
    ev[12..16].copy_from_slice(&event.to_le_bytes());
    ev[16..20].copy_from_slice(&child.to_le_bytes());
    ev[20..22].copy_from_slice(&root_x.to_le_bytes());
    ev[22..24].copy_from_slice(&root_y.to_le_bytes());
    ev[24..26].copy_from_slice(&event_x.to_le_bytes());
    ev[26..28].copy_from_slice(&event_y.to_le_bytes());
    ev[28..30].copy_from_slice(&state.to_le_bytes());
    ev[30] = if same_screen { 1 } else { 0 };
    ev
}

const fn build_keymap() -> [[u32; 2]; 248] {
    let mut map = [[0u32; 2]; 248];
    // KC 9 = Escape
    map[1] = [0xff1b, 0xff1b];
    // KC 10-19 = 1-0
    map[2] = [0x0031, 0x0021];
    map[3] = [0x0032, 0x0040];
    map[4] = [0x0033, 0x0023];
    map[5] = [0x0034, 0x0024];
    map[6] = [0x0035, 0x0025];
    map[7] = [0x0036, 0x005e];
    map[8] = [0x0037, 0x0026];
    map[9] = [0x0038, 0x002a];
    map[10] = [0x0039, 0x0028];
    map[11] = [0x0030, 0x0029];
    map[12] = [0x002d, 0x005f];
    map[13] = [0x003d, 0x002b];
    map[14] = [0xff08, 0xff08]; // BackSpace
    map[15] = [0xff09, 0xff09]; // Tab
    // KC 24-35 = q-]
    map[16] = [0x0071, 0x0051]; // q
    map[17] = [0x0077, 0x0057]; // w
    map[18] = [0x0065, 0x0045]; // e
    map[19] = [0x0072, 0x0052]; // r
    map[20] = [0x0074, 0x0054]; // t
    map[21] = [0x0079, 0x0059]; // y
    map[22] = [0x0075, 0x0055]; // u
    map[23] = [0x0069, 0x0049]; // i
    map[24] = [0x006f, 0x004f]; // o
    map[25] = [0x0070, 0x0050]; // p
    map[26] = [0x005b, 0x007b]; // [
    map[27] = [0x005d, 0x007d]; // ]
    map[28] = [0xff0d, 0xff0d]; // Return
    map[29] = [0xffe3, 0xffe3]; // Ctrl_L
    // KC 38-48 = a-;
    map[30] = [0x0061, 0x0041]; // a
    map[31] = [0x0073, 0x0053]; // s
    map[32] = [0x0064, 0x0044]; // d
    map[33] = [0x0066, 0x0046]; // f
    map[34] = [0x0067, 0x0047]; // g
    map[35] = [0x0068, 0x0048]; // h
    map[36] = [0x006a, 0x004a]; // j
    map[37] = [0x006b, 0x004b]; // k
    map[38] = [0x006c, 0x004c]; // l
    map[39] = [0x003b, 0x003a]; // ;
    map[40] = [0x0027, 0x0022]; // '
    map[41] = [0x0060, 0x007e]; // `
    map[42] = [0xffe1, 0xffe1]; // Shift_L
    map[43] = [0x005c, 0x007c]; // backslash
    // KC 52-61 = z-/
    map[44] = [0x007a, 0x005a]; // z
    map[45] = [0x0078, 0x0058]; // x
    map[46] = [0x0063, 0x0043]; // c
    map[47] = [0x0076, 0x0056]; // v
    map[48] = [0x0062, 0x0042]; // b
    map[49] = [0x006e, 0x004e]; // n
    map[50] = [0x006d, 0x004d]; // m
    map[51] = [0x002c, 0x003c]; // ,
    map[52] = [0x002e, 0x003e]; // .
    map[53] = [0x002f, 0x003f]; // /
    map[54] = [0xffe2, 0xffe2]; // Shift_R
    map[55] = [0xffaa, 0xffaa]; // KP_Multiply
    map[56] = [0xffe9, 0xffe9]; // Alt_L
    map[57] = [0x0020, 0x0020]; // space
    map[58] = [0xffe5, 0xffe5]; // Caps_Lock
    // F1-F10 (KC 67-76)
    map[59] = [0xffbe, 0xffbe]; // F1
    map[60] = [0xffbf, 0xffbf]; // F2
    map[61] = [0xffc0, 0xffc0]; // F3
    map[62] = [0xffc1, 0xffc1]; // F4
    map[63] = [0xffc2, 0xffc2]; // F5
    map[64] = [0xffc3, 0xffc3]; // F6
    map[65] = [0xffc4, 0xffc4]; // F7
    map[66] = [0xffc5, 0xffc5]; // F8
    map[67] = [0xffc6, 0xffc6]; // F9
    map[68] = [0xffc7, 0xffc7]; // F10
    map[69] = [0xff7f, 0xff7f]; // Num_Lock
    map[70] = [0xff14, 0xff14]; // Scroll_Lock
    // KP keys 79-91
    map[71] = [0xff95, 0xffb7]; // KP_7/Home
    map[72] = [0xff97, 0xffb8]; // KP_8/Up
    map[73] = [0xff9a, 0xffb9]; // KP_9/PgUp
    map[74] = [0xffad, 0xffad]; // KP_Subtract
    map[75] = [0xff96, 0xffb4]; // KP_4/Left
    map[76] = [0xff9d, 0xffb5]; // KP_5
    map[77] = [0xff98, 0xffb6]; // KP_6/Right
    map[78] = [0xffab, 0xffab]; // KP_Add
    map[79] = [0xff9c, 0xffb1]; // KP_1/End
    map[80] = [0xff99, 0xffb2]; // KP_2/Down
    map[81] = [0xff9b, 0xffb3]; // KP_3/PgDn
    map[82] = [0xff9e, 0xffb0]; // KP_0/Insert
    map[83] = [0xff9f, 0xffae]; // KP_Decimal/Delete
    // 92-93 unused
    map[86] = [0x003c, 0x003e]; // <, >  (KC 94)
    map[87] = [0xffc8, 0xffc8]; // F11
    map[88] = [0xffc9, 0xffc9]; // F12
    map[89] = [0xff50, 0xff50]; // Home
    map[90] = [0xff52, 0xff52]; // Up
    map[91] = [0xff55, 0xff55]; // Prior/PageUp
    map[92] = [0xff51, 0xff51]; // Left
    // 101 unused
    map[94] = [0xff53, 0xff53]; // Right
    map[95] = [0xff57, 0xff57]; // End
    map[96] = [0xff54, 0xff54]; // Down
    map[97] = [0xff56, 0xff56]; // Next/PageDown
    map[98] = [0xff63, 0xff63]; // Insert
    map[99] = [0xffff, 0xffff]; // Delete
    map[100] = [0xff8d, 0xff8d]; // KP_Enter
    map[101] = [0xffe4, 0xffe4]; // Ctrl_R
    map[103] = [0xffaf, 0xffaf]; // KP_Divide
    map[104] = [0xff61, 0xff61]; // Print
    map[105] = [0xffea, 0xffea]; // Alt_R
    map[107] = [0xff67, 0xff67]; // Menu
    map
}
