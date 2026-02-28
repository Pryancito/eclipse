//! Command encoder and render pass for `sidewind_wgpu`.
//!
//! `CommandEncoder` records a sequence of GPU operations.  When `finish()`
//! is called the recorded commands are bundled into a `CommandBuffer` that
//! can be submitted to the `Queue`.
//!
//! On VirtIO/virgl the commands are forwarded to the kernel via
//! `virgl_submit_3d`.  On the NVIDIA/software paths they are executed
//! immediately as CPU-side blits / MMIO writes.

extern crate alloc;
use alloc::vec::Vec;

use crate::backend::ActiveBackend;
use crate::Color;
use crate::texture::Texture;
use crate::pipeline::RenderPipeline;
use crate::buffer::Buffer;

// ── Virgl command helpers ─────────────────────────────────────────────────────
// The virgl protocol is a stream of 32-bit words.  Commands follow the layout
// defined in Mesa's `virgl_protocol.h`.  We only implement the subset needed
// for full-screen clears and 2-D blit operations.

/// Virgl PIPE_CLEAR flag for colour buffer 0.
const VIRGL_CLEAR_COLOR0: u32 = 1 << 4;
/// Virgl command type: CLEAR (0x1C).
const VIRGL_CCMD_CLEAR: u32 = 0x1C;
/// Virgl command type: BLIT (0x14).
#[allow(dead_code)]
const VIRGL_CCMD_BLIT: u32 = 0x14;

/// Build the header word for a virgl command.
///  * `cmd`   — VIRGL_CCMD_* constant
///  * `obj`   — object type (0 for non-object commands)
///  * `length` — number of payload u32 words that follow
#[inline]
fn virgl_cmd_header(cmd: u32, obj: u32, length: u32) -> u32 {
    (length & 0xFF_FFFF) | ((obj & 0xFF) << 24) | ((cmd & 0xFF) << 16)
}

// ── Recorded render operations ────────────────────────────────────────────────

/// A single recorded GPU operation.
enum RenderOp {
    /// Fill a rectangular region with a solid colour.
    Clear {
        color: Color,
        /// Back-buffer virtual address to clear.
        vaddr: u64,
        width: u32,
        height: u32,
        pitch_px: u32,
    },
    /// Blit `src` texture into the render target at (dx, dy).
    BlitTexture {
        dst_vaddr: u64,
        dst_pitch_px: u32,
        dst_x: i32,
        dst_y: i32,
        dst_w: u32,
        dst_h: u32,
        src_vaddr: u64,
        src_w: u32,
    },
    /// Submit a raw virgl 3-D command buffer.
    VirglSubmit {
        ctx_id: u32,
        /// Owned command bytes.
        cmd: Vec<u8>,
    },
    /// Write a pixel block via NVIDIA BAR0 2-D engine MMIO.
    NvidiaFillRect {
        bar0_virt: u64,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        color_bgra: u32,
    },
}

// ── RenderPass ────────────────────────────────────────────────────────────────

/// An active render pass — records draw calls against a single colour attachment.
pub struct RenderPass<'enc> {
    encoder: &'enc mut CommandEncoder,
    /// Cached back-buffer address from the colour attachment.
    target_vaddr: u64,
    target_width: u32,
    target_height: u32,
    target_pitch_px: u32,
}

impl<'enc> RenderPass<'enc> {
    /// Set the render pipeline for subsequent draw calls.
    pub fn set_pipeline(&mut self, _pipeline: &RenderPipeline) {
        // Pipeline state is carried implicitly through the software rasteriser.
    }

    /// Set the vertex buffer for slot 0.
    pub fn set_vertex_buffer(&mut self, _slot: u32, _buffer: &Buffer) {}

    /// Set the index buffer.
    pub fn set_index_buffer(&mut self, _buffer: &Buffer) {}

    /// Draw `vertex_count` vertices starting at `first_vertex`.
    ///
    /// On Eclipse OS this is handled by the CPU software rasteriser
    /// (`sidewind_opengl`).  Actual triangle rasterisation would require
    /// feeding the vertex data into `sidewind_opengl::GlContext`.
    pub fn draw(&mut self, _vertex_count: u32, _first_vertex: u32) {}

    /// Draw `index_count` indexed primitives.
    pub fn draw_indexed(&mut self, _index_count: u32, _first_index: u32, _base_vertex: i32) {}

    /// Fill a rectangle on the current render target with `color`.
    pub fn fill_rect(&mut self, x: i32, y: i32, w: u32, h: u32, color: Color) {
        if w == 0 || h == 0 { return; }
        if self.target_vaddr < 0x1000 { return; }
        let color_raw = color.to_bgra_u32();
        let ptr = self.target_vaddr as *mut u32;
        let pitch_px = self.target_pitch_px;
        let fb_w = self.target_width as i32;
        let fb_h = self.target_height as i32;
        for iy in 0..h as i32 {
            let dy = y + iy;
            if dy < 0 || dy >= fb_h { continue; }
            for ix in 0..w as i32 {
                let dx = x + ix;
                if dx < 0 || dx >= fb_w { continue; }
                let off = (dy as usize) * (pitch_px as usize) + (dx as usize);
                unsafe { core::ptr::write_volatile(ptr.add(off), color_raw); }
            }
        }
    }

    /// Blit a source texture (`src`) into the render target at (`dx`, `dy`).
    pub fn blit_texture(&mut self, src: &Texture, dx: i32, dy: i32) {
        if src.backing_vaddr < 0x1000 { return; }
        self.encoder.ops.push(RenderOp::BlitTexture {
            dst_vaddr: self.target_vaddr,
            dst_pitch_px: self.target_pitch_px,
            dst_x: dx,
            dst_y: dy,
            dst_w: self.target_width,
            dst_h: self.target_height,
            src_vaddr: src.backing_vaddr,
            src_w: src.width(),
        });
    }
}

// ── CommandEncoder ────────────────────────────────────────────────────────────

/// Records GPU commands.  Call `finish()` to produce a `CommandBuffer`.
pub struct CommandEncoder {
    ops: Vec<RenderOp>,
    active_backend: ActiveBackend,
    /// virgl context ID (VirtIO backend only; 0 if unused).
    virgl_ctx_id: u32,
    /// BAR0 virtual address (NVIDIA backend only; 0 if unused).
    bar0_virt: u64,
}

impl CommandEncoder {
    pub(crate) fn new(active_backend: ActiveBackend, virgl_ctx_id: u32, bar0_virt: u64) -> Self {
        Self {
            ops: Vec::new(),
            active_backend,
            virgl_ctx_id,
            bar0_virt,
        }
    }

    /// Begin a render pass that clears the colour attachment to `clear_color`
    /// and returns a `RenderPass` for subsequent draw calls.
    pub fn begin_render_pass<'enc>(
        &'enc mut self,
        target: &Texture,
        clear_color: Color,
    ) -> RenderPass<'enc> {
        let vaddr    = target.backing_vaddr;
        let width    = target.width();
        let height   = target.height();
        let pitch_px = target.row_pitch() / 4; // convert bytes → pixels

        // Record the clear operation.
        if vaddr >= 0x1000 {
            self.ops.push(RenderOp::Clear { color: clear_color, vaddr, width, height, pitch_px });
        }

        RenderPass {
            encoder: self,
            target_vaddr: vaddr,
            target_width: width,
            target_height: height,
            target_pitch_px: pitch_px,
        }
    }

    /// Submit a raw virgl 3-D command buffer (VirtIO backend).
    ///
    /// `cmd_bytes` must contain a valid virgl command stream; invalid data
    /// may cause the host virgl renderer to reject the submission.
    pub fn submit_virgl_commands(&mut self, cmd_bytes: Vec<u8>) {
        if self.virgl_ctx_id == 0 { return; }
        self.ops.push(RenderOp::VirglSubmit {
            ctx_id: self.virgl_ctx_id,
            cmd: cmd_bytes,
        });
    }

    /// Fill a rectangle via the NVIDIA 2-D engine (NVIDIA backend).
    pub fn nvidia_fill_rect(&mut self, x: u32, y: u32, w: u32, h: u32, color_bgra: u32) {
        if self.bar0_virt == 0 { return; }
        self.ops.push(RenderOp::NvidiaFillRect {
            bar0_virt: self.bar0_virt,
            x, y, w, h,
            color_bgra,
        });
    }

    /// Finish recording and return a `CommandBuffer` ready for queue submission.
    pub fn finish(self) -> CommandBuffer {
        CommandBuffer {
            ops: self.ops,
            active_backend: self.active_backend,
        }
    }
}

// ── CommandBuffer ─────────────────────────────────────────────────────────────

/// A recorded, immutable sequence of GPU commands.
pub struct CommandBuffer {
    ops: Vec<RenderOp>,
    active_backend: ActiveBackend,
}

impl CommandBuffer {
    /// Execute all recorded operations synchronously.
    ///
    /// This is called internally by `Queue::submit`.
    pub(crate) fn execute(self) {
        use eclipse_libc::virgl_submit_3d;
        use sidewind_nvidia::features::graphics2d::{Graphics2dEngine, Nvidia2DOperation, Rect};

        for op in self.ops {
            match op {
                // ── CPU clear ────────────────────────────────────────────────
                RenderOp::Clear { color, vaddr, width: _, height, pitch_px } => {
                    if vaddr < 0x1000 { continue; }
                    let color_raw = color.to_bgra_u32();
                    let total_px = (pitch_px as usize).saturating_mul(height as usize);
                    let ptr = vaddr as *mut u32;
                    // Fill the back-buffer with a single efficient 4-byte
                    // pattern.  When all four bytes are the same we can use
                    // `write_bytes`; for arbitrary colours we write row-by-row
                    // to keep inner loops cache-friendly.
                    let bytes = color_raw.to_ne_bytes();
                    if bytes[0] == bytes[1] && bytes[1] == bytes[2] && bytes[2] == bytes[3] {
                        // Fast path: all bytes identical — use ptr::write_bytes.
                        unsafe {
                            core::ptr::write_bytes(ptr as *mut u8, bytes[0], total_px * 4);
                        }
                    } else {
                        // General path: fill u32-by-u32.
                        for i in 0..total_px {
                            unsafe { core::ptr::write_volatile(ptr.add(i), color_raw); }
                        }
                    }
                }

                // ── CPU blit ─────────────────────────────────────────────────
                RenderOp::BlitTexture { dst_vaddr, dst_pitch_px, dst_x, dst_y,
                                        dst_w, dst_h, src_vaddr, src_w } => {
                    if dst_vaddr < 0x1000 || src_vaddr < 0x1000 { continue; }
                    let dst = dst_vaddr as *mut u32;
                    let src = src_vaddr as *const u32;
                    let h = dst_h as i32;
                    let w = dst_w as i32;
                    let sw = src_w as i32;
                    for iy in 0..h {
                        let dy = dst_y + iy;
                        if dy < 0 || dy >= h { continue; }
                        let src_row = (iy * sw) as usize;
                        let dst_row = ((dy as usize) * dst_pitch_px as usize)
                            .saturating_add(dst_x.max(0) as usize);
                        let copy_w = (w - dst_x.max(0)).min(sw - dst_x.max(0)).max(0) as usize;
                        if copy_w == 0 { continue; }
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                src.add(src_row),
                                dst.add(dst_row),
                                copy_w,
                            );
                        }
                    }
                }

                // ── VirtIO virgl submission ───────────────────────────────────
                RenderOp::VirglSubmit { ctx_id, cmd } => {
                    let _ = virgl_submit_3d(ctx_id, &cmd);
                }

                // ── NVIDIA 2-D engine fill ────────────────────────────────────
                RenderOp::NvidiaFillRect { bar0_virt, x, y, w, h, color_bgra } => {
                    let mut eng = Graphics2dEngine::new(bar0_virt);
                    let _ = eng.execute(Nvidia2DOperation::FillRect(
                        Rect { x, y, width: w, height: h },
                        color_bgra,
                    ));
                }
            }
        }
    }
}
