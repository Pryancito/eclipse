//! Pintado: lectura de la página, cromo con tiny-skia y búferes wl_shm.

use super::*;

impl App {
    pub(super) fn paint_page(&mut self) -> Result<(), String> {
        let Some(session) = self.session.as_ref() else {
            return Ok(());
        };
        let _ = session.context.make_current();
        session.shell.paint();
        if !self.shm_present {
            let scale = self.scale;
            if let Some(page) = &self.page {
                page.set_buffer_scale(scale);
            }
            session.context.present();
            if let Some(page) = &self.page {
                page.commit();
            }
            return Ok(());
        }
        let (pw, ph) = self.content_size();
        if !self.bgra_read {
            return self.paint_page_rgba(pw, ph);
        }
        let t0 = Instant::now();
        let gl = session.context.gleam_gl_api();
        session.context.prepare_for_rendering();
        gl.bind_vertex_array(0);
        let len = (pw as usize) * (ph as usize) * 4;
        let mut cur = std::mem::take(&mut self.readback);
        cur.resize(len, 0);
        gl.read_pixels_into_buffer(
            0,
            0,
            pw as i32,
            ph as i32,
            gleam::gl::BGRA,
            gleam::gl::UNSIGNED_BYTE,
            &mut cur,
        );
        if gl.get_error() != gleam::gl::NO_ERROR {
            eprintln!("silversurfer: glReadPixels(BGRA) no disponible; lectura RGBA");
            self.bgra_read = false;
            self.readback = cur;
            return self.paint_page_rgba(pw, ph);
        }
        self.log_readback(&cur, pw, ph, true);
        let t1 = Instant::now();
        // Un búfer nuevo (resize, primer fotograma) se pinta entero.
        let fresh = self.shown.len() != len;
        let rows = if fresh {
            Some((0, ph.saturating_sub(1)))
        } else {
            chrome::changed_rows_flipped(&cur, &self.shown, pw, ph)
        };
        let Some((top, bottom)) = rows else {
            // Idéntico a lo que ya se ve: ni copia ni commit.
            self.readback = cur;
            if let Some(stats) = self.stats.as_mut() {
                stats.add(t1 - t0, Duration::ZERO, 0, ph);
            }
            return Ok(());
        };
        let dst_y = (self.chrome_h() * self.scale).max(0) as u32;
        let full = (0, ph.saturating_sub(1));
        let since_back = match self.last_rows {
            Some(last) if !fresh => (top.min(last.0), bottom.max(last.1)),
            _ => full,
        };
        let copied = self.with_free_parent(|dst, dst_w, stride, has_page| {
            let copy = if has_page { since_back } else { full };
            chrome::blit_bgra_flipped(&cur, pw, ph, dst, dst_w, stride, dst_y, copy);
        });
        let t2 = Instant::now();
        if !copied {
            // Los dos búferes siguen en el compositor: reintentar tras el release.
            self.readback = cur;
            self.retry_frame();
            return Ok(());
        }
        self.commit_parent_rows(dst_y + top, bottom - top + 1)?;
        if let Some(stats) = self.stats.as_mut() {
            stats.add(t1 - t0, t2 - t1, bottom - top + 1, ph);
        }
        self.readback = std::mem::replace(&mut self.shown, cur);
        self.last_rows = Some((top, bottom));
        Ok(())
    }

    /// Camino antiguo: `read_to_image` (RGBA ya volteado) y daño completo.
    pub(super) fn paint_page_rgba(&mut self, pw: u32, ph: u32) -> Result<(), String> {
        let Some(session) = self.session.as_ref() else {
            return Ok(());
        };
        let rect =
            webrender_api::units::DeviceIntRect::from_size(euclid::size2(pw as i32, ph as i32));
        let t0 = Instant::now();
        let Some(image) = session.context.read_to_image(rect) else {
            return Ok(());
        };
        let raw = image.into_raw();
        let width = image_width(&raw, pw);
        self.log_readback(&raw, width, ph, false);
        let t1 = Instant::now();
        let dst_y = (self.chrome_h() * self.scale).max(0) as u32;
        // Este camino no lleva `shown`: el próximo BGRA copia entero.
        self.shown.clear();
        self.last_rows = None;
        let ok = self.with_free_parent(|dst, dst_w, stride, _| {
            chrome::blit_rgba(&raw, width, ph, dst, dst_w, stride, dst_y);
        });
        let t2 = Instant::now();
        if !ok {
            self.retry_frame();
            return Ok(());
        }
        if let Some(stats) = self.stats.as_mut() {
            stats.add(t1 - t0, t2 - t1, ph, ph);
        }
        let height = self.parent_buf.as_ref().map_or(0, |b| b.height as u32);
        self.commit_parent_rows(0, height)
    }

    /// Resume en el log los primeros fotogramas leídos: tamaño, color medio y
    /// cuántos píxeles son opacos. Distingue una lectura vacía (todo ceros)
    /// de una página que sí llega pero no se ve.
    pub(super) fn log_readback(&mut self, px: &[u8], width: u32, height: u32, bgra: bool) {
        if self.read_frames >= 3 {
            return;
        }
        self.read_frames += 1;
        let (mut sum, mut opaque, mut n) = ([0u64; 3], 0u64, 0u64);
        // Una muestra cada 7 píxeles basta y no cuesta nada.
        for p in px.chunks_exact(4).step_by(7) {
            let (r, g, b) = if bgra {
                (p[2], p[1], p[0])
            } else {
                (p[0], p[1], p[2])
            };
            sum[0] += u64::from(r);
            sum[1] += u64::from(g);
            sum[2] += u64::from(b);
            opaque += u64::from(p[3] == 255);
            n += 1;
        }
        let n = n.max(1);
        eprintln!(
            "silversurfer: fotograma {} leído {}x{} medio=#{:02x}{:02x}{:02x} opacos={}%",
            self.read_frames,
            width,
            height,
            sum[0] / n,
            sum[1] / n,
            sum[2] / n,
            opaque * 100 / n,
        );
    }

    /// Vuelve a marcar el fotograma como pendiente para pintarlo más tarde.
    pub(super) fn retry_frame(&self) {
        if let Some(session) = &self.session {
            session
                .shell
                .flags()
                .frame
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// El compositor aún no mostró el último fotograma de la página. Un
    /// callback perdido no puede congelar la página: pasado un segundo se
    /// pinta igual.
    pub(super) fn waiting_for_frame(&self) -> bool {
        self.frame_wait
            .is_some_and(|since| since.elapsed() < Duration::from_secs(1))
    }

    /// Deja en `parent_buf` un búfer libre del tamaño del toplevel y le pasa
    /// su memoria a `fill`. `false` si no hay ninguno libre todavía.
    pub(super) fn with_free_parent(
        &mut self,
        fill: impl FnOnce(&mut [u8], u32, usize, bool),
    ) -> bool {
        // A resize in the same dispatch would otherwise blit into the
        // old-sized buffer, which paint_chrome then replaces with a bare panel.
        if self.ensure_parent_buffer().is_err() || !self.free_parent_buffer() {
            return false;
        }
        let Some(buf) = self.parent_buf.as_mut() else {
            return false;
        };
        let stride = (buf.width as usize) * 4;
        let len = buf.mem.len;
        let dst = unsafe { std::slice::from_raw_parts_mut(buf.mem.ptr, len) };
        fill(dst, buf.width as u32, stride, buf.page);
        buf.page = true;
        true
    }

    pub(super) fn paint_chrome(&mut self) -> Result<(), String> {
        let titles;
        let loading;
        let icons;
        let active;
        let location;
        let scheme;
        let window_title;
        let can_back;
        let can_forward;
        if let Some(session) = self.session.as_ref() {
            titles = session.shell.tab_titles();
            loading = session.shell.tab_loading();
            icons = session.shell.tab_icons();
            active = session.shell.active_index();
            can_back = session.shell.can_go_back();
            can_forward = session.shell.can_go_forward();
            window_title = session.shell.window_title();
            location = if self.loc_focused {
                self.loc.text().to_owned()
            } else {
                session.shell.location_text()
            };
            scheme = if self.loc_focused {
                SchemeHint::Other
            } else {
                SchemeHint::from_url_str(&location)
            };
        } else {
            titles = vec!["SilverSurfer".into()];
            loading = vec![false];
            icons = Vec::new();
            active = 0;
            can_back = false;
            can_forward = false;
            location = String::new();
            scheme = SchemeHint::Other;
            window_title = "SilverSurfer".into();
        }
        let gpu = self.session.as_ref().is_some_and(|s| s.gpu);
        let private = self.session.as_ref().is_some_and(|s| s.private);
        let window_title = if private && !window_title.contains("Privado") {
            format!("Privado — {window_title}")
        } else {
            window_title
        };
        if let Some(top) = &self.toplevel {
            top.set_title(window_title);
        }
        let suggest = if self.loc_focused && !self.find_mode {
            history::load()
                .autocomplete(&location)
                .map(|v| v.url.chars().skip(location.chars().count()).collect())
                .unwrap_or_default()
        } else {
            String::new()
        };
        let popup = self
            .session
            .as_ref()
            .and_then(|s| s.shell.context_popup())
            .map(|p| p.clamp_to(self.logical_w, self.logical_h));
        let view = View {
            width: self.logical_w,
            look: self.look,
            titles: &titles,
            loading: &loading,
            icons: &icons,
            active,
            location: &location,
            location_focused: self.loc_focused,
            location_cursor: self.loc.cursor(),
            location_selection: self.loc.selection(),
            location_suggest: &suggest,
            find_mode: self.find_mode,
            scheme,
            gpu,
            private,
            can_back,
            can_forward,
            scale: self.scale,
        };
        let fp = chrome_fingerprint(
            &titles,
            &loading,
            &icons,
            active,
            &location,
            self.loc_focused,
            self.loc.cursor(),
            self.loc.selection(),
            self.find_mode,
            scheme,
            gpu,
            can_back,
            can_forward,
            self.scale,
            self.logical_w,
            popup.as_ref(),
        );
        if fp == self.chrome_fp && popup.is_none() {
            // Contenido idéntico: no reconstruir ni recommitear el wl_shm del cromo.
            return Ok(());
        }
        let pixels = menu::render(&view, self.logical_h, popup.as_ref());
        self.upload_chrome(&pixels.argb, pixels.width, pixels.height)?;
        self.chrome_fp = fp;
        self.ensure_parent_buffer()?;
        if let Some(page) = &self.page {
            page.set_buffer_scale(self.scale);
        }
        Ok(())
    }

    pub(super) fn upload_chrome(
        &mut self,
        argb: &[u8],
        width: i32,
        height: i32,
    ) -> Result<(), String> {
        let scale = self.scale;
        let surface = self
            .chrome_surface
            .clone()
            .ok_or("superficie del cromo ausente")?;
        let buf = self.buffer(width, height, true)?;
        copy_into(buf, argb);
        surface.set_buffer_scale(scale);
        surface.attach(Some(&buf.buffer), 0, 0);
        surface.damage_buffer(0, 0, width, height);
        surface.commit();
        buf.busy = true;
        Ok(())
    }

    pub(super) fn ensure_parent_buffer(&mut self) -> Result<(), String> {
        let width = self.logical_w * self.scale;
        let height = self.logical_h * self.scale;
        // A busy buffer still holds the page that was just blitted. Replacing
        // it here would commit a panel-coloured frame over that page.
        if self
            .parent_buf
            .as_ref()
            .is_some_and(|buf| buf.width == width && buf.height == height)
        {
            return Ok(());
        }
        let scale = self.scale;
        let look = self.look;
        let surface = self.parent.clone().ok_or("superficie ausente")?;
        let buf = self.buffer(width, height, false)?;
        fill_panel(buf, look);
        surface.set_buffer_scale(scale);
        surface.attach(Some(&buf.buffer), 0, 0);
        surface.damage_buffer(0, 0, width, height);
        surface.commit();
        buf.busy = true;
        // Lo que se ve ahora es el panel: el próximo fotograma va entero.
        self.shown.clear();
        self.last_rows = None;
        Ok(())
    }

    /// Leaves a released buffer of the toplevel's size in `parent_buf`,
    /// swapping in (or allocating) the back buffer when the front one is
    /// still held by the compositor. `false` when both are held: the page is
    /// retried once a `wl_buffer.release` arrives.
    pub(super) fn free_parent_buffer(&mut self) -> bool {
        let Some(front) = self.parent_buf.as_ref() else {
            return false;
        };
        if !front.busy {
            return true;
        }
        let (width, height) = (front.width, front.height);
        let back_fits = self
            .parent_back
            .as_ref()
            .is_some_and(|buf| buf.width == width && buf.height == height);
        if !back_fits {
            if let Some(old) = self.parent_back.take() {
                if old.busy {
                    self.retired.push(old);
                }
            }
            let Some(shm) = self.shm.clone() else {
                return false;
            };
            let id = self.next_buf;
            self.next_buf += 1;
            match alloc_buffer(&shm, &self.qh, width, height, id) {
                Ok(mut buf) => {
                    fill_panel(&mut buf, self.look);
                    self.parent_back = Some(buf);
                }
                Err(err) => {
                    eprintln!("silversurfer: búfer trasero de la página: {err}");
                    return false;
                }
            }
        }
        if self.parent_back.as_ref().is_some_and(|buf| buf.busy) {
            return false;
        }
        std::mem::swap(&mut self.parent_buf, &mut self.parent_back);
        true
    }

    /// Adjunta `parent_buf` marcando como dañadas solo `rows` filas desde
    /// `y` (píxeles de búfer), y pide un `frame` para no adelantarse al
    /// compositor.
    pub(super) fn commit_parent_rows(&mut self, y: u32, rows: u32) -> Result<(), String> {
        let Some(surface) = self.parent.clone() else {
            return Ok(());
        };
        let qh = self.qh.clone();
        let Some(buf) = self.parent_buf.as_mut() else {
            return Ok(());
        };
        surface.attach(Some(&buf.buffer), 0, 0);
        surface.damage_buffer(0, y as i32, buf.width, rows as i32);
        surface.frame(&qh, ());
        surface.commit();
        buf.busy = true;
        self.frame_wait = Some(Instant::now());
        Ok(())
    }

    /// `chrome == true` stores the buffer as the toolbar's. Otherwise it is
    /// the toplevel's.
    pub(super) fn buffer(
        &mut self,
        width: i32,
        height: i32,
        chrome: bool,
    ) -> Result<&mut LiveBuf, String> {
        let slot = if chrome {
            &mut self.chrome_buf
        } else {
            &mut self.parent_buf
        };
        let reuse = slot
            .as_ref()
            .is_some_and(|buf| buf.width == width && buf.height == height && !buf.busy);
        if !reuse && !chrome {
            // A resize also invalidates the software page's back buffer.
            if let Some(old) = self.parent_back.take() {
                if old.busy {
                    self.retired.push(old);
                }
            }
        }
        let slot = if chrome {
            &mut self.chrome_buf
        } else {
            &mut self.parent_buf
        };
        if !reuse {
            if let Some(old) = slot.take() {
                if old.busy {
                    self.retired.push(old);
                }
            }
            let shm = self.shm.as_ref().ok_or("wl_shm ausente")?.clone();
            let id = self.next_buf;
            self.next_buf += 1;
            *slot = Some(alloc_buffer(&shm, &self.qh, width, height, id)?);
        }
        Ok(slot.as_mut().expect("el búfer acaba de crearse"))
    }

    pub(super) fn release_buffer(&mut self, id: u64) {
        for buf in [
            &mut self.parent_buf,
            &mut self.parent_back,
            &mut self.chrome_buf,
        ] {
            if let Some(live) = buf.as_mut() {
                if live.id == id {
                    live.busy = false;
                }
            }
        }
        self.retired.retain(|buf| buf.id != id);
    }
}

/// Medias del camino software (lectura GL, copia a wl_shm, filas dañadas),
/// impresas cada 120 fotogramas.
pub(super) struct FrameStats {
    frames: u32,
    read: Duration,
    copy: Duration,
    damaged_rows: u64,
    total_rows: u64,
    since: Instant,
}

impl FrameStats {
    pub(super) fn from_env() -> Option<Self> {
        std::env::var_os("SILVERSURFER_FRAME_STATS")
            .filter(|v| v != "0")
            .map(|_| Self {
                frames: 0,
                read: Duration::ZERO,
                copy: Duration::ZERO,
                damaged_rows: 0,
                total_rows: 0,
                since: Instant::now(),
            })
    }

    pub(super) fn add(&mut self, read: Duration, copy: Duration, damaged: u32, rows: u32) {
        self.frames += 1;
        self.read += read;
        self.copy += copy;
        self.damaged_rows += u64::from(damaged);
        self.total_rows += u64::from(rows);
        if self.frames < 120 {
            return;
        }
        let n = f64::from(self.frames);
        let secs = self.since.elapsed().as_secs_f64().max(1e-3);
        eprintln!(
            "silversurfer: frames={} fps={:.1} lectura_ms={:.2} copia_ms={:.2} daño={:.0}%",
            self.frames,
            n / secs,
            self.read.as_secs_f64() * 1e3 / n,
            self.copy.as_secs_f64() * 1e3 / n,
            100.0 * self.damaged_rows as f64 / self.total_rows.max(1) as f64,
        );
        self.frames = 0;
        self.read = Duration::ZERO;
        self.copy = Duration::ZERO;
        self.damaged_rows = 0;
        self.total_rows = 0;
        self.since = Instant::now();
    }
}

pub(super) fn chrome_fingerprint(
    titles: &[String],
    loading: &[bool],
    icons: &[Option<Rc<chrome::Favicon>>],
    active: usize,
    location: &str,
    loc_focused: bool,
    loc_cursor: usize,
    loc_selection: Option<(usize, usize)>,
    find_mode: bool,
    scheme: SchemeHint,
    gpu: bool,
    can_back: bool,
    can_forward: bool,
    scale: i32,
    width: i32,
    popup: Option<&menu::Popup>,
) -> u64 {
    // FNV-1a 64: suficiente para saltar commits idénticos del cromo.
    let mut h: u64 = 0xcbf29ce484222325;
    let mut mix = |bytes: &[u8]| {
        for &b in bytes {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x100000001b3);
        }
    };
    mix(&(width as u32).to_le_bytes());
    mix(&(scale as u32).to_le_bytes());
    mix(&(active as u32).to_le_bytes());
    mix(&[u8::from(loc_focused), u8::from(find_mode), u8::from(gpu)]);
    mix(&[u8::from(can_back), u8::from(can_forward)]);
    mix(&(loc_cursor as u32).to_le_bytes());
    if let Some((a, b)) = loc_selection {
        mix(&(a as u32).to_le_bytes());
        mix(&(b as u32).to_le_bytes());
    }
    mix(&[match scheme {
        SchemeHint::Https => 1,
        SchemeHint::Http => 2,
        SchemeHint::File => 3,
        SchemeHint::Other => 0,
    }]);
    mix(location.as_bytes());
    for icon in icons {
        // La dirección del Rc cambia con cada favicon nuevo.
        let id = icon.as_ref().map_or(0, |i| Rc::as_ptr(i) as usize as u64);
        mix(&id.to_le_bytes());
    }
    for (t, l) in titles
        .iter()
        .zip(loading.iter().chain(std::iter::repeat(&false)))
    {
        mix(t.as_bytes());
        mix(&[u8::from(*l)]);
        mix(&[0xff]);
    }
    if let Some(p) = popup {
        mix(&(p.x as u32).to_le_bytes());
        mix(&(p.y as u32).to_le_bytes());
        for item in &p.items {
            mix(item.label.as_bytes());
            mix(&[u8::from(item.enabled), u8::from(item.separator)]);
        }
    }
    h
}

pub(super) fn copy_into(buf: &mut LiveBuf, src: &[u8]) {
    let len = buf.mem.len.min(src.len());
    unsafe {
        std::ptr::copy_nonoverlapping(src.as_ptr(), buf.mem.ptr, len);
    }
}

pub(super) fn fill_panel(buf: &mut LiveBuf, look: Look) {
    let rgb = look.palette().panel;
    let px = [rgb.b, rgb.g, rgb.r, 255];
    let len = buf.mem.len;
    let dst = unsafe { std::slice::from_raw_parts_mut(buf.mem.ptr, len) };
    for chunk in dst.chunks_exact_mut(4) {
        chunk.copy_from_slice(&px);
    }
    buf.page = false;
}

pub(super) fn image_width(raw: &[u8], fallback: u32) -> u32 {
    let pixels = (raw.len() / 4) as u32;
    if pixels == 0 {
        return fallback;
    }
    // `read_to_image` is the content size we asked for. Trust the caller when
    // the buffer divides evenly by that width.
    if fallback > 0 && pixels % fallback == 0 {
        fallback
    } else {
        fallback.max(1)
    }
}

pub(super) fn alloc_buffer(
    shm: &WlShm,
    qh: &QueueHandle<App>,
    width: i32,
    height: i32,
    id: u64,
) -> Result<LiveBuf, String> {
    let width = width.max(1);
    let height = height.max(1);
    let stride = width * 4;
    let len = (stride as usize)
        .checked_mul(height as usize)
        .ok_or("búfer demasiado grande")?;
    let name = std::ffi::CString::new("silversurfer").unwrap();
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err("memfd_create".into());
    }
    if unsafe { libc::ftruncate(fd, len as libc::off_t) } != 0 {
        unsafe { libc::close(fd) };
        return Err("ftruncate".into());
    }
    let map = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        )
    };
    if map == libc::MAP_FAILED {
        unsafe { libc::close(fd) };
        return Err("mmap".into());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let pool = shm.create_pool(file.as_fd(), len as i32, qh, ());
    let buffer = pool.create_buffer(0, width, height, stride, wl_shm::Format::Argb8888, qh, id);
    drop(file);
    Ok(LiveBuf {
        buffer,
        _pool: pool,
        mem: ShmMem {
            ptr: map.cast(),
            len,
        },
        id,
        busy: false,
        width,
        height,
        page: false,
    })
}
