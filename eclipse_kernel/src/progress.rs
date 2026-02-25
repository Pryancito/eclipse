use embedded_graphics::{
    pixelcolor::Rgb888,
    prelude::*,
    primitives::{Rectangle, PrimitiveStyleBuilder},
    text::Text,
    mono_font::{ascii::{FONT_6X10, FONT_10X20}, MonoTextStyle, MonoTextStyleBuilder},
};
use crate::boot::{get_fb_info, FbSource, VIRTIO_DISPLAY_RESOURCE_ID};
use spin::Mutex;

// Buffer estático para acumular líneas de log hasta recibir '\n'
const LOG_BUF_SIZE: usize = 128;
struct LogBuffer {
    buf: [u8; LOG_BUF_SIZE],
    len: usize,
}
impl LogBuffer {
    const fn new() -> Self {
        Self { buf: [0u8; LOG_BUF_SIZE], len: 0 }
    }
    fn push_str(&mut self, s: &str) -> Option<usize> {
        // Returns Some(pos_of_newline) si hay \n en el string dado
        let mut newline_pos = None;
        for b in s.bytes() {
            if b == b'\n' {
                newline_pos = Some(self.len);
                // No guardamos el \n en el buffer
            } else {
                if self.len < LOG_BUF_SIZE {
                    self.buf[self.len] = b;
                    self.len += 1;
                }
            }
        }
        newline_pos
    }
    fn flush(&mut self) -> &str {
        let s = core::str::from_utf8(&self.buf[..self.len]).unwrap_or("");
        s
    }
    fn clear(&mut self) {
        self.len = 0;
    }
}
static LOG_BUFFER: Mutex<LogBuffer> = Mutex::new(LogBuffer::new());

/// Kernel framebuffer wrapper for embedded-graphics DrawTarget
pub struct KernelFramebuffer {
    ptr: *mut u8,
    width: u32,
    height: u32,
    pitch: u32,
}

impl KernelFramebuffer {
    pub fn new(ptr: *mut u8, width: u32, height: u32, pitch: u32) -> Self {
        Self { ptr, width, height, pitch }
    }

    /// Write pixel at (x, y) in BGRA format (VirtIO/UEFI typical)
    #[inline]
    fn write_pixel(&mut self, x: i32, y: i32, color: Rgb888) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }
        let offset = (y as u32 * self.pitch + x as u32 * 4) as isize;
        unsafe {
            let p = self.ptr.offset(offset);
            *p = color.b();
            *p.offset(1) = color.g();
            *p.offset(2) = color.r();
            *p.offset(3) = 0xFF;
        }
    }
}

impl OriginDimensions for KernelFramebuffer {
    fn size(&self) -> Size {
        Size::new(self.width, self.height)
    }
}

impl DrawTarget for KernelFramebuffer {
    type Color = Rgb888;
    type Error = core::convert::Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        for Pixel(coord, color) in pixels {
            self.write_pixel(coord.x, coord.y, color);
        }
        Ok(())
    }
}

pub fn bar(progress: u32) {
    let Some((phys, width, height, pitch, source)) = get_fb_info() else { return };
    let virt = crate::memory::phys_to_virt(phys) as *mut u8;
    let mut fb = KernelFramebuffer::new(virt, width, height, pitch);
    
    let progress = progress.min(100);

    let bar_width = 400;
    let bar_height = 20;
    let x = (width as i32 - bar_width as i32) / 2;
    let y = (height as i32 - bar_height as i32) / 2;

    // Draw outer border
    let _ = Rectangle::new(Point::new(x - 2, y - 2), Size::new(bar_width + 4, bar_height + 4))
        .into_styled(PrimitiveStyleBuilder::new()
            .stroke_color(Rgb888::WHITE)
            .stroke_width(1)
            .build())
        .draw(&mut fb);

    // Draw inner progress
    let fill_width = (bar_width * progress) / 100;
    if fill_width > 0 {
        let _ = Rectangle::new(Point::new(x, y), Size::new(fill_width, bar_height))
            .into_styled(PrimitiveStyleBuilder::new()
                .fill_color(Rgb888::WHITE)
                .build())
            .draw(&mut fb);
    }
    
    // Fill remaining with black
    if fill_width < bar_width {
         let _ = Rectangle::new(Point::new(x + fill_width as i32, y), Size::new(bar_width - fill_width, bar_height))
            .into_styled(PrimitiveStyleBuilder::new()
                .fill_color(Rgb888::BLACK)
                .build())
            .draw(&mut fb);
    }

    // Text "XX%" below the bar, fixed width to avoid jumping
    let mut s_buf = [b' '; 4];
    s_buf[3] = b'%';
    
    if progress == 100 {
        s_buf[0] = b'1'; s_buf[1] = b'0'; s_buf[2] = b'0';
    } else if progress >= 10 {
        s_buf[1] = b'0' + (progress / 10) as u8;
        s_buf[2] = b'0' + (progress % 10) as u8;
    } else {
        s_buf[2] = b'0' + progress as u8;
    }
    
    let s = core::str::from_utf8(&s_buf).unwrap_or(" ? %");

    let text_style = MonoTextStyleBuilder::new()
        .font(&FONT_6X10)
        .text_color(Rgb888::WHITE)
        .background_color(Rgb888::BLACK)
        .build();
    let text_width = 4 * 6;
    let _ = Text::new(s, Point::new(x + (bar_width as i32 - text_width) / 2, y + bar_height as i32 + 15), text_style)
        .draw(&mut fb);

    // VirtIO GPU requires explicit present
    if source == FbSource::VirtIO {
        let _ = crate::virtio::gpu_present(VIRTIO_DISPLAY_RESOURCE_ID, 0, 0, width, height);
    }
}

/// Renderiza en pantalla el contenido del buffer. Llamar solo con el buffer ya completado.
fn render_log_line(line: &str, source: FbSource, width: u32, height: u32, pitch: u32, phys: u64) {
    let virt = crate::memory::phys_to_virt(phys) as *mut u8;
    let mut fb = KernelFramebuffer::new(virt, width, height, pitch);

    let bar_width = 400u32;
    let bar_height = 20i32;
    let x = (width as i32 - bar_width as i32) / 2;
    let y = (height as i32 - bar_height) / 2;

    // Posición del texto: ENCIMA de la barra
    let log_y = y - 10;

    let text_style = MonoTextStyleBuilder::new()
        .font(&FONT_6X10)
        .text_color(Rgb888::WHITE)
        .background_color(Rgb888::BLACK)
        .build();

    // Limpiar área del mensaje anterior
    let _ = Rectangle::new(Point::new(x, log_y - 10), Size::new(bar_width, 12))
        .into_styled(PrimitiveStyleBuilder::new()
            .fill_color(Rgb888::BLACK)
            .build())
        .draw(&mut fb);

    const LOG_CHAR_LIMIT: usize = 64;
    let truncated = if line.len() > LOG_CHAR_LIMIT {
        &line[line.len() - LOG_CHAR_LIMIT..]
    } else {
        line
    };

    let _ = Text::new(truncated, Point::new(x, log_y), text_style)
        .draw(&mut fb);

    // VirtIO GPU requiere present explícito
    if source == FbSource::VirtIO {
        let _ = crate::virtio::gpu_present(VIRTIO_DISPLAY_RESOURCE_ID, 0, (log_y - 12) as u32, bar_width, 24);
    }
}

/// Acumula `msg` en el buffer de línea. Solo renderiza en pantalla cuando llega un '\n'.
pub fn log(msg: &str) {
    let mut buf = LOG_BUFFER.lock();
    let got_newline = buf.push_str(msg);

    if got_newline.is_some() {
        // Sin alloc: copiamos el contenido del buffer a un array en el stack
        const MAX: usize = LOG_BUF_SIZE;
        let mut tmp = [0u8; MAX];
        let line_bytes = buf.flush().as_bytes();
        let n = line_bytes.len().min(MAX);
        tmp[..n].copy_from_slice(&line_bytes[..n]);
        let line = core::str::from_utf8(&tmp[..n]).unwrap_or("");

        // Obtener info del framebuffer y renderizar.
        // get_fb_info() no usa LOG_BUFFER, así que no hay deadlock.
        if let Some((phys, width, height, pitch, source)) = get_fb_info() {
            buf.clear();
            drop(buf);
            render_log_line(line, source, width, height, pitch, phys);
        } else {
            buf.clear();
        }
    }
    // Si no hay '\n', el texto queda en el buffer esperando el siguiente fragmento.
}

// ============================================================================
// BSOD (Blue Screen Of Death)
// ============================================================================

/// Datos que necesita el BSOD para renderizar. Se pasan por valor para evitar
/// referencias a la pila de excepción durante el renderizado.
pub struct BsodInfo {
    pub exception_num: u64,
    pub error_code:    u64,
    pub rip: u64,
    pub rsp: u64,
    pub cr2: u64,
    pub cr3: u64,
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub r8:  u64,
    pub r9:  u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rflags: u64,
    pub cs: u64,
    pub ss: u64,
    pub cpu_id: u64,
    pub pid: u64,
}

/// Nombre legible de la excepción x86-64
fn exception_name(num: u64) -> &'static str {
    match num {
        0  => "DIVIDE_BY_ZERO (#DE)",
        1  => "DEBUG (#DB)",
        2  => "NMI",
        3  => "BREAKPOINT (#BP)",
        4  => "OVERFLOW (#OF)",
        5  => "BOUND_RANGE (#BR)",
        6  => "INVALID_OPCODE (#UD)",
        7  => "DEVICE_NOT_AVAILABLE (#NM)",
        8  => "DOUBLE_FAULT (#DF)",
        10 => "INVALID_TSS (#TS)",
        11 => "SEGMENT_NOT_PRESENT (#NP)",
        12 => "STACK_SEGMENT (#SS)",
        13 => "GENERAL_PROTECTION_FAULT (#GP)",
        14 => "PAGE_FAULT (#PF)",
        16 => "FPU_FLOAT_POINT (#MF)",
        17 => "ALIGNMENT_CHECK (#AC)",
        18 => "MACHINE_CHECK (#MC)",
        19 => "SIMD_FLOAT_POINT (#XM)",
        _  => "UNKNOWN_EXCEPTION",
    }
}

/// Escribe un u64 como hex en un buffer `[u8; 18]` con prefijo "0x".
fn fmt_hex(v: u64, out: &mut [u8; 18]) {
    out[0] = b'0';
    out[1] = b'x';
    for i in 0..16 {
        let nibble = ((v >> (60 - i * 4)) & 0xF) as u8;
        out[2 + i] = if nibble < 10 { b'0' + nibble } else { b'a' + nibble - 10 };
    }
}

/// Pinta una pantalla BSOD completa con la información de la excepción.
/// Solo usa buffers de pila — sin alloc ni fmt!.
pub fn bsod(info: &BsodInfo) {
    let Some((phys, width, height, pitch, source)) = get_fb_info() else { return };
    let virt = crate::memory::phys_to_virt(phys) as *mut u8;
    let mut fb = KernelFramebuffer::new(virt, width, height, pitch);

    // 1. Limpiar pantalla: azul BSOD (#0078D7)
    let bsod_blue = Rgb888::new(0x00, 0x78, 0xD7);
    let _ = Rectangle::new(Point::new(0, 0), Size::new(width, height))
        .into_styled(PrimitiveStyleBuilder::new().fill_color(bsod_blue).build())
        .draw(&mut fb);

    // 2. Estilos de texto
    let style_big = MonoTextStyleBuilder::new()
        .font(&FONT_10X20)
        .text_color(Rgb888::WHITE)
        .background_color(bsod_blue)
        .build();
    let style_small = MonoTextStyleBuilder::new()
        .font(&FONT_6X10)
        .text_color(Rgb888::WHITE)
        .background_color(bsod_blue)
        .build();

    let margin_x = (width as i32) / 8;
    let mut y = (height as i32) / 6;

    // 3. Emoticon grande
    let _ = Text::new(":(", Point::new(margin_x, y), style_big).draw(&mut fb);
    y += 40;

    // 4. Titulo
    let _ = Text::new("Your PC ran into a problem and needs to restart.", Point::new(margin_x, y), style_big).draw(&mut fb);
    y += 30;
    let _ = Text::new("Eclipse OS kernel exception.", Point::new(margin_x, y), style_big).draw(&mut fb);
    y += 50;

    // 5. Stop code
    let _ = Text::new("Stop code:", Point::new(margin_x, y), style_small).draw(&mut fb);
    y += 14;
    let _ = Text::new(exception_name(info.exception_num), Point::new(margin_x, y), style_big).draw(&mut fb);
    y += 40;

    // 6. Registros — sin alloc, usando buffers en pila
    let mut hex = [0u8; 18];

    macro_rules! draw_kv {
        ($label:literal, $val:expr) => {{
            fmt_hex($val, &mut hex);
            let mut buf = [b' '; 46];
            let lbl: &[u8] = $label.as_bytes();
            let ll = lbl.len().min(20);
            buf[..ll].copy_from_slice(&lbl[..ll]);
            buf[ll] = b':';
            buf[ll+1] = b' ';
            let hb = &hex[..18];
            buf[ll+2..ll+2+18].copy_from_slice(hb);
            let line = core::str::from_utf8(&buf[..ll+2+18]).unwrap_or("");
            let _ = Text::new(line, Point::new(margin_x, y), style_small).draw(&mut fb);
            y += 13;
        }};
    }

    macro_rules! draw_dec {
        ($label:literal, $val:expr) => {{
            let v: u64 = $val;
            let mut dec_buf = [b'0'; 20];
            let mut tmp = v;
            let mut dec_len = 1usize;
            if tmp > 0 {
                dec_len = 0;
                while tmp > 0 {
                    dec_buf[19 - dec_len] = b'0' + (tmp % 10) as u8;
                    tmp /= 10;
                    dec_len += 1;
                }
            }
            let dec_str = core::str::from_utf8(&dec_buf[20-dec_len..20]).unwrap_or("?");
            let mut buf = [b' '; 46];
            let lbl: &[u8] = ($label as &str).as_bytes();
            let ll = lbl.len().min(20);
            buf[..ll].copy_from_slice(&lbl[..ll]);
            buf[ll] = b':';
            buf[ll+1] = b' ';
            let db = dec_str.as_bytes();
            let dl = db.len().min(20);
            buf[ll+2..ll+2+dl].copy_from_slice(&db[..dl]);
            let line = core::str::from_utf8(&buf[..ll+2+dl]).unwrap_or("");
            let _ = Text::new(line, Point::new(margin_x, y), style_small).draw(&mut fb);
            y += 13;
        }};
    }

    draw_dec!("CPU",        info.cpu_id);
    draw_dec!("PID",        info.pid);
    draw_kv!("RIP",         info.rip);
    draw_kv!("RSP",         info.rsp);
    draw_kv!("Error Code",  info.error_code);
    draw_kv!("CR2",         info.cr2);
    draw_kv!("CR3",         info.cr3);
    y += 4;
    draw_kv!("RAX",         info.rax);
    draw_kv!("RBX",         info.rbx);
    draw_kv!("RCX",         info.rcx);
    draw_kv!("RDX",         info.rdx);
    draw_kv!("RSI",         info.rsi);
    draw_kv!("RDI",         info.rdi);
    draw_kv!("RBP",         info.rbp);
    y += 4;
    draw_kv!("R8 ",         info.r8);
    draw_kv!("R9 ",         info.r9);
    draw_kv!("R10",         info.r10);
    draw_kv!("R11",         info.r11);
    draw_kv!("R12",         info.r12);
    draw_kv!("R13",         info.r13);
    draw_kv!("R14",         info.r14);
    draw_kv!("R15",         info.r15);
    y += 4;
    draw_kv!("RFLAGS",      info.rflags);
    draw_kv!("CS ",         info.cs);
    draw_kv!("SS ",         info.ss);

    // 7. Flush si VirtIO
    if source == FbSource::VirtIO {
        let _ = crate::virtio::gpu_present(VIRTIO_DISPLAY_RESOURCE_ID, 0, 0, width, height);
    }
}
