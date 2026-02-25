use embedded_graphics::{
    pixelcolor::Rgb888,
    prelude::*,
    primitives::{Rectangle, PrimitiveStyleBuilder},
    text::Text,
    mono_font::{ascii::FONT_6X10, MonoTextStyle, MonoTextStyleBuilder},
};
use crate::FramebufferInfo;

struct UefiFramebuffer {
    ptr: *mut u8,
    width: u32,
    height: u32,
    pitch: u32,
}

impl UefiFramebuffer {
    fn new(info: &FramebufferInfo) -> Self {
        Self {
            ptr: info.base_address as *mut u8,
            width: info.width,
            height: info.height,
            pitch: info.pixels_per_scan_line * 4,
        }
    }

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

impl OriginDimensions for UefiFramebuffer {
    fn size(&self) -> Size {
        Size::new(self.width, self.height)
    }
}

impl DrawTarget for UefiFramebuffer {
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

pub fn bar(progress: u32, info: &FramebufferInfo) {
    if info.base_address == 0 || info.base_address == 0xDEADBEEF {
        return;
    }

    let mut fb = UefiFramebuffer::new(info);
    let progress = progress.min(100);

    let bar_width = 400;
    let bar_height = 20;
    let x = (info.width as i32 - bar_width as i32) / 2;
    let y = (info.height as i32 - bar_height as i32) / 2;

    // Clear background (optional as per request, but good for redrawing)
    // Actually, user said "white bar on black background", so let's clear at least the area if needed.
    // However, if it's a "progress bar", we might just want to draw the bar itself.
    
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
    
    // Fill remaining with black (to handle decreasing progress or just clean refresh)
    if fill_width < bar_width {
         let _ = Rectangle::new(Point::new(x + fill_width as i32, y), Size::new(bar_width - fill_width, bar_height))
            .into_styled(PrimitiveStyleBuilder::new()
                .fill_color(Rgb888::BLACK)
                .build())
            .draw(&mut fb);
    }

    // Text "XX%" below the bar, fixed width to avoid jumping
    let mut buf = [b' '; 4];
    buf[3] = b'%';
    if progress == 100 {
        buf[0] = b'1';
        buf[1] = b'0';
        buf[2] = b'0';
    } else if progress >= 10 {
        buf[1] = b'0' + (progress / 10) as u8;
        buf[2] = b'0' + (progress % 10) as u8;
    } else {
        buf[2] = b'0' + progress as u8;
    }
    
    let s = unsafe { core::str::from_utf8_unchecked(&buf) };

    let text_style = MonoTextStyleBuilder::new()
        .font(&FONT_6X10)
        .text_color(Rgb888::WHITE)
        .background_color(Rgb888::BLACK)
        .build();
    let text_width = s.len() as i32 * 6;
    let _ = Text::new(s, Point::new(x + (bar_width as i32 - text_width) / 2, y + bar_height as i32 + 15), text_style)
        .draw(&mut fb);
}

pub fn log(msg: &str, info: &FramebufferInfo) {
    if info.base_address == 0 || info.base_address == 0xDEADBEEF {
        return;
    }

    let mut fb = UefiFramebuffer::new(info);
    
    let bar_width = 400;
    let bar_height = 20;
    let x = (info.width as i32 - bar_width as i32) / 2;
    let y = (info.height as i32 - bar_height as i32) / 2;

    // Position text ABOVE the bar
    let log_y = y - 10;
    
    let text_style = MonoTextStyleBuilder::new()
        .font(&FONT_6X10)
        .text_color(Rgb888::WHITE)
        .background_color(Rgb888::BLACK)
        .build();

    // Clear a small area for the last message to avoid overlap
    let _ = Rectangle::new(Point::new(x, log_y - 10), Size::new(bar_width, 12))
        .into_styled(PrimitiveStyleBuilder::new()
            .fill_color(Rgb888::BLACK)
            .build())
        .draw(&mut fb);

    const LOG_CHAR_LIMIT: usize = 64;

    // Filter out newlines/carriage returns for single line display
    let clean_msg = msg.trim_end_matches(|c| c == '\r' || c == '\n');
    let truncated_msg = if clean_msg.len() > LOG_CHAR_LIMIT {
        &clean_msg[clean_msg.len() - LOG_CHAR_LIMIT..]
    } else {
        clean_msg
    };

    let _ = Text::new(truncated_msg, Point::new(x, log_y), text_style)
        .draw(&mut fb);
}
