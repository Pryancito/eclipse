//! The cursor compositor: the patch blit, the blend into the scanout and
//! the repaint from the panel copy. Out of `drm.rs` for its size ceiling.

use super::*;

/// Move the software pointer on the panel WITHOUT reading the client's
/// framebuffer: erase it by putting back what it was covering, and draw it over
/// what the panel itself holds at the new position.
///
/// This is the fix for the black rectangles. See [`CursorUnder`] for what went
/// wrong and why the panel is the only honest source for a pointer move: the
/// client's buffer stopped being the frame on screen the moment the compositor
/// started the next one in it, which our present invites it to do.
///
/// Returns false, having touched nothing, when the panel cannot be read back --
/// anything but ARGB8888, see [`DisplayScheme::read_into`] -- in which case the
/// caller's older read-from-the-client path runs. That is what such a panel has
/// today, and it is not one any machine this kernel runs on has: a UEFI GOP,
/// virtio-gpu and an NVIDIA BAR1 aperture are all 32-bit.
pub(super) fn repaint_cursor_from_panel(
    display: &dyn DisplayScheme,
    old_drawn: Option<(i32, i32, u32, u32)>,
    new: Option<(i32, i32, u32, u32, &[u32])>,
) -> bool {
    if cursor_from_client() || !display.fb_readable() {
        return false;
    }
    // Take the pointer off the panel first, and only from the save: the panel at
    // the old rect holds the pointer BLENDED over the scene, and an opaque
    // pointer pixel cannot be un-blended. Erasing before reading is also what
    // lets the draw below read the panel at all -- when the two rects overlap,
    // reading first would capture the old pointer and blend the new one over it,
    // baking a trail in.
    {
        let mut u = CURSOR_UNDER.lock();
        // Only while `drawn` still describes what the save was taken for. See
        // [`CursorUnder::for_cursor`].
        if u.for_cursor == old_drawn {
            if let Some((x, y, w, h)) = u.rect {
                // `px` holds exactly `w * h` pixels, `w` per row: `save_cursor_under`
                // wrote it from the window it is describing.
                display.blit_from(x, y, &u.px, w as usize, w, h);
            }
        }
        u.rect = None;
        u.for_cursor = None;
    }
    let Some((cx, cy, cw, ch, bmp)) = new else {
        return true;
    };
    let info = display.info();
    // The window the blit will write: clipped to the panel and widened to the
    // write-combining boundary. `read_into` clamps a window exactly as
    // `blit_from` does, so what is read back is what will be written -- which is
    // the property the restore above depends on.
    let y0 = cy.max(0);
    let y1 = (cy + ch as i32).min(info.height as i32);
    let x0 = cx.max(0) as u32;
    let x1 = (cx + cw as i32).max(0) as u32;
    let (wx, ww) = expand_x_for_wc(x0, x1.saturating_sub(x0), info.pitch() / 4);
    if y1 <= y0 || ww == 0 {
        // Wholly off screen. Nothing is drawn and nothing is covered, which the
        // erase above has already recorded.
        return true;
    }
    let rows = (y1 - y0) as usize;
    let tw = ww as usize;
    let need = tw.saturating_mul(rows);
    let mut slot = CURSOR_PATCH.lock();
    if slot.len() < need {
        slot.resize(need, 0);
    }
    let patch = &mut slot[..need];
    if !display.read_into(wx, y0 as u32, patch, tw, ww, rows as u32) {
        // The window is there but the panel would not give it up. Leave the
        // pointer undrawn rather than compose over whatever the scratch buffer
        // held from the last move, which is a stale square of an older frame --
        // the same rule the `rows` bookkeeping in `blit_cursor_patch` follows.
        return true;
    }
    save_cursor_under((wx, y0 as u32, tw, rows), patch, (cx, cy, cw, ch));
    blend_cursor_into(patch, (wx as i32, y0, tw, rows), (cx, cy, cw, ch), bmp);
    display.blit_from(wx, y0 as u32, patch, tw, ww, rows as u32);
    true
}

/// Copy `patch` of the CRTC fb, blend the cursor into it in RAM, blit once.
#[allow(clippy::too_many_arguments)]
pub(super) fn blit_cursor_patch(
    display: &dyn DisplayScheme,
    pixels: &[u32],
    src_stride: usize,
    fw: u32,
    fh: u32,
    px: i32,
    py: i32,
    pw: u32,
    ph: u32,
    cx: i32,
    cy: i32,
    cw: u32,
    ch: u32,
    bmp: &[u32],
    copied: Option<(u32, u32, u32, u32)>,
) {
    if src_stride == 0 || pw == 0 || ph == 0 {
        return;
    }
    let dinfo = display.info();
    let pitch_px = image_pitch_px(src_stride, dinfo.pitch() / 4, dinfo.width, fw);
    let x0 = px.max(0) as u32;
    let y0 = py.max(0);
    let x1 = (px + pw as i32).max(0) as u32;
    let y1 = (py + ph as i32).min(fh as i32);
    let width = x1.saturating_sub(x0);
    if y1 <= y0 || width == 0 {
        return;
    }
    let (x0, tw_u) = expand_x_for_wc(x0, width, pitch_px);
    if tw_u == 0 {
        return;
    }
    // Everything the pointer was drawn into, which is wider than what this
    // framebuffer covers when the client's is smaller than the mode. Asked before
    // `CURSOR_PATCH` is taken so the two locks keep their order (PATCH then
    // UNDER): this call holds only UNDER.
    let (x0, y0, tw_u, wide_rows) = match widen_window_to_what_was_drawn(
        (x0, y0.max(0) as u32, tw_u as usize, (y1 - y0) as usize),
        (cx, cy, cw, ch),
        copied,
        dinfo.pitch() / 4,
        dinfo.height,
    ) {
        Some((sx, sy, sw, sh)) => {
            #[cfg(test)]
            CURSOR_WINDOWS_WIDENED.fetch_add(1, Ordering::Relaxed);
            (sx, sy as i32, sw as u32, Some(sh))
        }
        None => (x0, y0, tw_u, None),
    };
    let x0 = x0 as i32;
    let tw = tw_u as usize;
    let th = wide_rows.unwrap_or((y1 - y0) as usize);
    let need = tw.saturating_mul(th);
    let mut slot = CURSOR_PATCH.lock();
    if slot.len() < need {
        slot.resize(need, 0);
    }
    let patch = &mut slot[..need];
    // The background the pointer goes on top of, from two sources chosen by
    // geometry and not by trust -- [`patch_outside_copy_from_save`] says why. The
    // save goes first, for the whole window, so the buffer only has to fill the
    // part this present made the panel equal to it.
    let from_save = match copied {
        Some(rect) => patch_outside_copy_from_save(
            patch,
            (x0 as u32, y0 as u32, tw, th),
            (cx, cy, cw, ch),
            rect,
        ),
        None => 0,
    };
    #[cfg(test)]
    CURSOR_PX_FROM_SAVE.store(from_save, Ordering::Relaxed);
    // Rows actually sourced. CURSOR_PATCH is scratch REUSED across moves, so a row
    // nothing could fill still holds the previous patch's pixels -- blitting `th`
    // rows unconditionally would paint that onto the screen as a stale square.
    let mut rows = 0usize;
    if from_save > 0 {
        // The save filled every pixel outside the copied rectangle, and the
        // rectangle itself is inside what the framebuffer covers, so the whole
        // window has its background and only its copied part comes from the
        // buffer.
        rows = th;
        let (rx, ry, rw, rh) = copied.expect("from_save > 0 needs a copied rect");
        let bx0 = rx.max(x0 as u32);
        let bx1 = (rx + rw).min(x0 as u32 + tw as u32);
        let by0 = ry.max(y0 as u32);
        let by1 = (ry + rh).min(y0 as u32 + th as u32);
        for y in by0..by1 {
            let src_off = (y as usize)
                .saturating_mul(src_stride)
                .saturating_add(bx0 as usize);
            let n = (bx1.saturating_sub(bx0) as usize)
                .min(src_stride.saturating_sub(bx0 as usize))
                .min(pixels.len().saturating_sub(src_off));
            if n == 0 {
                continue;
            }
            let dst_off = (y - y0 as u32) as usize * tw + (bx0 - x0 as u32) as usize;
            patch[dst_off..dst_off + n].copy_from_slice(&pixels[src_off..src_off + n]);
        }
    } else {
        for r in 0..th {
            let src_y = y0 as usize + r;
            let src_off = src_y.saturating_mul(src_stride).saturating_add(x0 as usize);
            let dst_off = r * tw;
            let n = tw.min(pixels.len().saturating_sub(src_off));
            if n < tw {
                break;
            }
            rows = r + 1;
            patch[dst_off..dst_off + n].copy_from_slice(&pixels[src_off..src_off + n]);
        }
    }
    if rows == 0 {
        // Nothing was drawn, so nothing is covered. Said explicitly because a
        // save left over from the previous position would otherwise be restored
        // onto a panel this pointer is not on -- a stale square, which is the
        // defect the `rows` bookkeeping above exists to avoid causing.
        forget_cursor_under();
        return;
    }
    // Save what the pointer is about to cover BEFORE blending it in, so the next
    // move can put it back instead of asking the client's framebuffer what used
    // to be there. See [`CursorUnder`].
    //
    // These are the panel's own pixels whatever the client did, and not because
    // the buffer is trustworthy: the blit below writes this whole window to the
    // panel, so the panel is MADE to hold what was just saved. On a pointer move,
    // where nothing writes the window first, that guarantee is gone -- which is
    // the whole difference between this path and `repaint_cursor_from_panel`.
    save_cursor_under(
        (x0 as u32, y0 as u32, tw, rows),
        &patch[..rows * tw],
        (cx, cy, cw, ch),
    );
    blend_cursor_into(patch, (x0, y0, tw, rows), (cx, cy, cw, ch), bmp);
    display.blit_from(x0 as u32, y0 as u32, patch, tw, tw as u32, rows as u32);
}

/// Blend the pointer bitmap over `patch`, a `tw` x `rows` window of the scene
/// whose top-left corner is at `(x0, y0)` on screen.
///
/// wlroots renders cursors with premultiplied alpha, so the operator is
/// `out = src + dst * (255 - a) / 255`; fully transparent pixels are skipped and
/// fully opaque ones are written without reading what they cover. One function
/// and not two because the pointer is composited from two different sources --
/// the frame a present is putting up, and the panel itself on a move -- and a
/// blend written out twice is a blend that can disagree with itself.
pub(super) fn blend_cursor_into(
    patch: &mut [u32],
    window: (i32, i32, usize, usize),
    cursor: (i32, i32, u32, u32),
    bmp: &[u32],
) {
    let (x0, y0, tw, rows) = window;
    let (cx, cy, cw, ch) = cursor;
    // The contract, checked once rather than per pixel: both callers size the
    // patch as `tw * rows` exactly, and a smaller one would have the blend run
    // off the end of it.
    if patch.len() < tw.saturating_mul(rows) {
        return;
    }
    for r in 0..rows {
        let dst_off = r * tw;
        let cr = (y0 + r as i32) - cy;
        if cr < 0 || cr >= ch as i32 {
            continue;
        }
        let bmp_row = cr as usize * cw as usize;
        for c in 0..tw {
            let cc = (x0 + c as i32) - cx;
            if cc < 0 || cc >= cw as i32 {
                continue;
            }
            let si = bmp_row + cc as usize;
            if si >= bmp.len() {
                break;
            }
            let s = bmp[si];
            let a = s >> 24;
            if a == 0 {
                continue;
            }
            let di = dst_off + c;
            patch[di] = if a == 0xff {
                s | 0xFF00_0000
            } else {
                let d = patch[di];
                let inv = 255 - a;
                let (sr, sg, sb) = ((s >> 16) & 0xff, (s >> 8) & 0xff, s & 0xff);
                let (dr, dg, db) = ((d >> 16) & 0xff, (d >> 8) & 0xff, d & 0xff);
                let or = (sr + dr * inv / 255).min(0xff);
                let og = (sg + dg * inv / 255).min(0xff);
                let ob = (sb + db * inv / 255).min(0xff);
                0xFF00_0000 | (or << 16) | (og << 8) | ob
            };
        }
    }
}
