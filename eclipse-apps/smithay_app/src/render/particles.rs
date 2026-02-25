use embedded_graphics::prelude::*;
use embedded_graphics::pixelcolor::Rgb888;
use super::FramebufferState;

pub const MAX_PARTICLES: usize = 128;

#[derive(Clone, Copy)]
pub struct Particle {
    pub x: f32,
    pub y: f32,
    pub vx: f32,
    pub vy: f32,
    pub life: f32, // 1.0 to 0.0
    pub color: Rgb888,
}

pub struct ParticleManager {
    pub particles: [Option<Particle>; MAX_PARTICLES],
    pub next_idx: usize,
    pub rng_seed: u32,
}

impl ParticleManager {
    pub fn new() -> Self {
        Self {
            particles: [None; MAX_PARTICLES],
            next_idx: 0,
            rng_seed: 0x12345,
        }
    }

    fn next_rand(&mut self) -> f32 {
        self.rng_seed = self.rng_seed.wrapping_mul(1103515245).wrapping_add(12345);
        let res = (self.rng_seed & 0x7FFFFFFF) as f32 / 0x7FFFFFFF as f32;
        res
    }

    pub fn update(&mut self, cursor: (i32, i32), velocity: f32, accent: Rgb888) {
        // Emit particles based on velocity
        let emission_count = (velocity * 2.0) as usize;
        for _ in 0..emission_count {
            let vx = (self.next_rand() - 0.5) * 4.0;
            let vy = (self.next_rand() - 0.5) * 4.0;

            let p_color = accent;

            self.particles[self.next_idx] = Some(Particle {
                x: cursor.0 as f32,
                y: cursor.1 as f32,
                vx,
                vy,
                life: 1.0,
                color: p_color,
            });
            self.next_idx = (self.next_idx + 1) % MAX_PARTICLES;
        }

        // Update existing particles (pre-compute rand values to avoid double borrow)
        let mut rands = [(0.0f32, 0.0f32); MAX_PARTICLES];
        for i in 0..MAX_PARTICLES {
            rands[i] = (self.next_rand(), self.next_rand());
        }
        for (i, p) in self.particles.iter_mut().enumerate() {
            if let Some(ref mut part) = p {
                part.x += part.vx;
                part.y += part.vy;
                part.life -= 0.02;

                if part.life <= 0.0 {
                    *p = None;
                }
            }
        }
    }

    pub fn draw(&self, fb: &mut FramebufferState) {
        for p in self.particles.iter() {
            if let Some(part) = p {
                let x = part.x as i32;
                let y = part.y as i32;

                // Fade color based on life
                let alpha = part.life;
                let final_color = Rgb888::new(
                    (part.color.r() as f32 * alpha) as u8,
                    (part.color.g() as f32 * alpha) as u8,
                    (part.color.b() as f32 * alpha) as u8,
                );

                // Draw 2x2 particle for volume
                fb.draw_pixel_raw(x, y, final_color);
                fb.draw_pixel_raw(x + 1, y, final_color);
                fb.draw_pixel_raw(x, y + 1, final_color);
                fb.draw_pixel_raw(x + 1, y + 1, final_color);
            }
        }
    }
}
