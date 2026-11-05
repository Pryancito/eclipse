//! A paste that runs while the browser stays usable.
//!
//! Pasting used to copy on the thread that draws the window, so copying a
//! 4 GB disk image froze lunarfile until it was done, with nothing on the
//! screen to say how far it had got or a way to stop it. Now the copy (or a
//! move across disks, which is a copy) runs on a thread of its own and counts
//! into a shared [`fileops::Progress`]; the main loop wakes ten times a second
//! while it runs to redraw a segmented bar in the footer; Esc stops it, and a
//! stopped copy leaves nothing behind. Browsing goes on meanwhile.
//!
//! A child of `lunarfile.rs` to keep that file under its size ceiling (see
//! `xtask/src/tamano.rs`).

use super::*;
use lunarbar::fileops::{self, Progress};
use std::sync::atomic::Ordering;
use std::sync::Arc;

pub struct Job {
    /// The thread, or none in a `--dump` that only draws a job.
    pub handle: Option<std::thread::JoinHandle<std::io::Result<std::path::PathBuf>>>,
    pub prog: Arc<Progress>,
    /// What is being pasted, as the footer names it.
    pub name: String,
    pub cut: bool,
    /// The folder it is going into, so the listing is re-read only if that is
    /// still the one on screen when it ends.
    pub dir: String,
}

impl Job {
    pub fn running(&self) -> bool {
        self.handle.as_ref().is_none_or(|h| !h.is_finished())
    }

    /// The footer line: `Copiando foto.iso · 45% · 120 M de 260 M · Esc cancela`.
    pub fn line(&self) -> String {
        let (d, t) = (self.prog.done.load(Ordering::Relaxed), self.prog.total.load(Ordering::Relaxed));
        let verb = match (self.cut, self.prog.cancel.load(Ordering::Relaxed)) {
            (_, true) => "Cancelando",
            (true, _) => "Moviendo",
            (false, _) => "Copiando",
        };
        format!(
            "{verb} {} · {}% · {} de {} · Esc cancela",
            self.name,
            (self.prog.frac() * 100.0).floor() as u32,
            files::human_size(d),
            files::human_size(t)
        )
    }
}

/// The bar's segments.
pub const SEGMENTS: i32 = 24;
pub const SEG_W: i32 = 4;

impl State {
    /// Start pasting what is held into the folder on screen.
    pub(crate) fn start_paste(&mut self) {
        if self.job.is_some() {
            self.note = "Espera a que termine la copia en curso (Esc la cancela)".into();
            return;
        }
        let Some((src, cut)) = self.held.clone() else { return };
        let prog = Arc::new(Progress::default());
        let (p, dir) = (prog.clone(), self.dir.clone());
        let (s, d) = (std::path::PathBuf::from(&src), std::path::PathBuf::from(&dir));
        let handle = std::thread::spawn(move || {
            if cut {
                fileops::move_into_with(&s, &d, &p)
            } else {
                fileops::copy_into_with(&s, &d, &p)
            }
        });
        let name = src.rsplit('/').next().unwrap_or(&src).to_string();
        self.note.clear();
        self.job = Some(Job { handle: Some(handle), prog, name, cut, dir });
    }

    /// Esc while a paste runs: ask it to stop.
    pub(crate) fn cancel_job(&mut self) -> bool {
        match &self.job {
            Some(j) if j.running() => {
                j.prog.cancel.store(true, Ordering::Relaxed);
                true
            }
            _ => false,
        }
    }

    /// Called by the loop: when the paste has ended, say how, and show it.
    /// True when there is something new to draw.
    pub(crate) fn tick_job(&mut self) -> bool {
        let Some(job) = &self.job else { return false };
        if job.running() {
            return job.handle.is_some();
        }
        let Some(job) = self.job.take() else { return false };
        let Some(h) = job.handle else { return false };
        match h.join() {
            Ok(Ok(dst)) => {
                let n = dst.file_name().map(|n| n.to_string_lossy().into_owned());
                if job.cut {
                    self.held = None;
                }
                if self.dir == job.dir {
                    self.reload_on(n.as_deref());
                }
                self.note = format!("Pegado: {}", n.unwrap_or_default());
            }
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::Interrupted => {
                self.note = format!("Cancelado: {}", job.name);
            }
            Ok(Err(e)) => self.note = format!("No se pudo pegar: {e}"),
            Err(_) => self.note = "La copia se detuvo por un error".into(),
        }
        true
    }
}

/// The footer's bar: [`SEGMENTS`] blocks, lit in the accent as far as the
/// copy has got, warning-coloured while it is being cancelled.
pub fn draw_bar(cv: &mut Canvas, st: &State, job: &Job, x: i32, y: i32) -> i32 {
    let p = st.pal;
    let lit = (job.prog.frac() * SEGMENTS as f32).floor() as i32;
    let on = if job.prog.cancel.load(Ordering::Relaxed) { WARN } else { p.accent };
    for i in 0..SEGMENTS {
        let c = if i < lit { on } else { p.border };
        cv.fill_rect_a(x + i * (SEG_W + 1), y, SEG_W, GLYPH_H - 4, c, 1.0);
    }
    SEGMENTS * (SEG_W + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_paste_runs_on_its_own_thread_and_ends_where_it_was_started() {
        let base = std::env::temp_dir().join(format!("lunarfile-job-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("dst")).unwrap();
        std::fs::write(base.join("f.bin"), vec![7u8; 2 << 20]).unwrap();
        let c = Cli { mode: Some(Mode::Browse), dir: Some(base.to_string_lossy().into_owned()), ..Cli::default() };
        let mut st = new_state(&c);
        st.held = Some((base.join("f.bin").to_string_lossy().into_owned(), false));
        st.chdir(&base.join("dst").to_string_lossy());
        st.start_paste();
        assert!(st.job.is_some());
        st.start_paste();
        assert!(st.note.starts_with("Espera"), "one paste at a time");
        while st.job.as_ref().is_some_and(|j| j.running()) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(st.tick_job());
        assert!(st.job.is_none());
        assert_eq!(st.note, "Pegado: f.bin");
        assert!(base.join("dst/f.bin").exists());
        assert_eq!(st.entry(st.sel).map(|e| e.name.as_str()), Some("f.bin"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_footer_says_how_far_and_how_to_stop() {
        let prog = Arc::new(Progress::default());
        prog.total.store(200 << 20, Ordering::Relaxed);
        prog.done.store(90 << 20, Ordering::Relaxed);
        let j = Job { handle: None, prog: prog.clone(), name: "disco.iso".into(), cut: false, dir: "/".into() };
        assert!(j.running());
        assert_eq!(j.line(), "Copiando disco.iso · 45% · 90 M de 200 M · Esc cancela");
        prog.cancel.store(true, Ordering::Relaxed);
        assert!(j.line().starts_with("Cancelando"));
    }
}
