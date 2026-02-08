//! Logo drawing module for Display Service
//!
//! Handles drawing the Eclipse OS logo to the framebuffer.
//! The logo is expected to be an 800x250 raw BGRA image.

pub const LOGO_WIDTH: u32 = 800;
pub const LOGO_HEIGHT: u32 = 250;
pub const LOGO_DATA: &[u8] = include_bytes!("logo.raw");

/// Draw the logo centered on the screen
/// 
/// # Arguments
/// * `fb_base` - Base address of the framebuffer
/// * `fb_pitch` - Bytes per scanline of the framebuffer
/// * `fb_bpp` - Bits per pixel of the framebuffer
/// * `screen_width` - Width of the screen in pixels
/// * `screen_height` - Height of the screen in pixels
pub fn draw(fb_base: usize, fb_pitch: u32, fb_bpp: u32, screen_width: u32, screen_height: u32) {
    // Only support 32-bpp for now as our generic logo is 32-bit BGRA
    if fb_bpp != 32 {
        return;
    }

    // Calculate centered position
    let start_x = (screen_width.saturating_sub(LOGO_WIDTH)) / 2;
    let start_y = (screen_height.saturating_sub(LOGO_HEIGHT)) / 2;
    
    let fb_ptr = fb_base as *mut u32;
    
    // Safety: modifying raw memory at framebuffer address
    unsafe {
        for y in 0..LOGO_HEIGHT {
            // Calculate offsets
            let screen_y = start_y + y;
            if screen_y >= screen_height {
                break;
            }
            
            let screen_offset = (screen_y * (fb_pitch / 4)) + start_x;
            let logo_offset = (y * LOGO_WIDTH) as usize;
            
            for x in 0..LOGO_WIDTH {
                let screen_x = start_x + x;
                if screen_x >= screen_width {
                    break;
                }
                
                // Get pixel from logo data (BGRA format)
                let pixel_idx = (logo_offset + x as usize) * 4;
                if pixel_idx + 3 >= LOGO_DATA.len() {
                    break; 
                }
                
                let b = LOGO_DATA[pixel_idx];
                let g = LOGO_DATA[pixel_idx + 1];
                let r = LOGO_DATA[pixel_idx + 2];
                let a = LOGO_DATA[pixel_idx + 3];
                
                // Simple alpha blending with black background (since we just cleared to black)
                // Or just overwrite if alpha is high enough
                if a > 0 {
                    let color = ((a as u32) << 24) | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
                    
                    // Write to framebuffer
                    // Note: pitch is in bytes, but we're indexing u32, so divide by 4
                    core::ptr::write_volatile(fb_ptr.add((screen_offset + x) as usize), color);
                }
            }
        }
    }
}
