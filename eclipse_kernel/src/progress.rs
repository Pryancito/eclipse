use embedded_graphics::{
    pixelcolor::Rgb888,
    prelude::*,
    primitives::{Rectangle, PrimitiveStyleBuilder},
    text::Text,
    mono_font::{ascii::FONT_6X10, MonoTextStyle, MonoTextStyleBuilder},
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
