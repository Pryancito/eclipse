//! What lunarfile shows of the selected file beside its list: the first lines
//! of a text, a PNG scaled to fit, what a folder holds, and for anything else
//! its kind, size, date and permissions.
//!
//! Read once per selection and kept (see `lunarfile`'s `State::preview`), and
//! bounded every way a file can be large: a text is read up to [`TEXT_BYTES`],
//! a picture up to [`IMAGE_BYTES`] and [`IMAGE_DIM`] on a side, a folder up to
//! [`DIR_SCAN`] names. A preview of a 4 GB disk image costs one `stat`.

use std::os::unix::fs::{MetadataExt, PermissionsExt};

use tiny_skia::{FilterQuality, Pixmap, PixmapPaint, Transform};

use crate::files::{self, Kind};
use crate::i18n::{Key, Lang};

/// How much of a text is read: enough for every line the column can show.
pub const TEXT_BYTES: usize = 8 * 1024;
/// How many lines of a text are kept.
pub const TEXT_LINES: usize = 48;
/// The largest PNG that is decoded.
pub const IMAGE_BYTES: u64 = 16 * 1024 * 1024;
/// The largest side of a PNG that is decoded.
pub const IMAGE_DIM: u32 = 8192;
/// How many names of a folder are counted.
pub const DIR_SCAN: usize = 10_000;
/// How many of a folder's names are kept to list.
pub const DIR_NAMES: usize = 16;

/// What the preview shows under its heading.
pub enum Body {
    /// A folder: how many folders and files it holds, how much the files
    /// weigh, and its first names (folders first). `more` is true when the
    /// count stopped at [`DIR_SCAN`].
    Dir { dirs: usize, files: usize, bytes: u64, names: Vec<(String, bool)>, more: bool },
    /// The first lines of a text, tabs expanded; `more` when it goes on.
    Text { lines: Vec<String>, more: bool },
    /// A picture, already scaled into the box it was asked for, and the size
    /// it really is.
    Image { pm: Pixmap, w: u32, h: u32 },
    /// Anything that is neither: only the details are shown.
    Plain,
    /// It could not be read (no permission, gone since the listing).
    Unreadable,
}

/// The selected entry, as the preview column shows it.
pub struct Preview {
    pub path: String,
    pub name: String,
    pub is_dir: bool,
    pub kind: Kind,
    pub size: u64,
    pub mtime: Option<i64>,
    /// `rwxr-xr-x`, when the filesystem said.
    pub mode: Option<String>,
    /// Where a symlink points.
    pub link: Option<String>,
    pub body: Body,
}

/// `rwxr-xr-x` for the low nine bits of a mode.
pub fn perms(mode: u32) -> String {
    let mut s = String::with_capacity(9);
    for shift in [6, 3, 0] {
        let b = (mode >> shift) & 7;
        s.push(if b & 4 != 0 { 'r' } else { '-' });
        s.push(if b & 2 != 0 { 'w' } else { '-' });
        s.push(if b & 1 != 0 { 'x' } else { '-' });
    }
    s
}

/// The name of a kind, as the preview's details say it.
pub fn kind_label(kind: Kind, is_dir: bool, lang: Lang) -> &'static str {
    if is_dir {
        return lang.s(Key::file_folder);
    }
    lang.s(match kind {
        Kind::Image => Key::file_kind_image,
        Kind::Document => Key::file_kind_document,
        Kind::Archive => Key::file_kind_archive,
        Kind::Disk => Key::file_kind_disk,
        Kind::Media => Key::file_kind_media,
        Kind::Code => Key::file_kind_code,
        Kind::Other => Key::file_kind_other,
    })
}

/// Whether `head` (the start of a file) reads as text: no NUL byte, and UTF-8
/// up to where the read may have cut a character in half.
pub fn is_text(head: &[u8]) -> bool {
    if head.contains(&0) {
        return false;
    }
    match std::str::from_utf8(head) {
        Ok(_) => true,
        // A cut in the last 3 bytes is the read's, not the file's.
        Err(e) => e.error_len().is_none() && e.valid_up_to() + 4 > head.len(),
    }
}

/// The lines of a text's start: `\r` dropped, tabs to four spaces, at most
/// [`TEXT_LINES`]; true when there is more than was kept.
pub fn text_lines(head: &[u8], cut: bool) -> (Vec<String>, bool) {
    let s = match std::str::from_utf8(head) {
        Ok(s) => s,
        Err(e) => std::str::from_utf8(&head[..e.valid_up_to()]).unwrap_or(""),
    };
    let mut lines: Vec<String> = s
        .split('\n')
        .map(|l| l.trim_end_matches('\r').replace('\t', "    "))
        .collect();
    // A text that ends in a newline has no line after it.
    if lines.last().is_some_and(|l| l.is_empty()) && !cut {
        lines.pop();
    }
    let more = cut || lines.len() > TEXT_LINES;
    lines.truncate(TEXT_LINES);
    (lines, more)
}

/// Scale `src` to fit `bw` x `bh`, never up: a 16 px icon stays 16 px.
fn fit(src: &Pixmap, bw: u32, bh: u32) -> Option<Pixmap> {
    let (w, h) = (src.width() as f32, src.height() as f32);
    let s = (bw as f32 / w).min(bh as f32 / h).min(1.0);
    let (dw, dh) = (((w * s).round() as u32).max(1), ((h * s).round() as u32).max(1));
    let mut dst = Pixmap::new(dw, dh)?;
    let paint = PixmapPaint { quality: FilterQuality::Bilinear, ..PixmapPaint::default() };
    dst.draw_pixmap(0, 0, src.as_ref(), &paint, Transform::from_scale(s, s), None);
    Some(dst)
}

fn image(path: &str, size: u64, bw: u32, bh: u32) -> Option<Body> {
    if size > IMAGE_BYTES || !path.to_ascii_lowercase().ends_with(".png") {
        return None;
    }
    let src = Pixmap::decode_png(&std::fs::read(path).ok()?).ok()?;
    if src.width() > IMAGE_DIM || src.height() > IMAGE_DIM {
        return None;
    }
    let (w, h) = (src.width(), src.height());
    Some(Body::Image { pm: fit(&src, bw, bh)?, w, h })
}

fn folder(path: &str) -> Body {
    let Ok(rd) = std::fs::read_dir(path) else {
        return Body::Unreadable;
    };
    let (mut dirs, mut nfiles, mut bytes, mut more) = (0, 0, 0u64, false);
    let mut names = Vec::new();
    for (i, e) in rd.flatten().enumerate() {
        if i >= DIR_SCAN {
            more = true;
            break;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        // `metadata` follows a link, as the list does.
        let md = std::fs::metadata(e.path()).ok();
        let is_dir = md.as_ref().is_some_and(|m| m.is_dir());
        if is_dir {
            dirs += 1;
        } else {
            nfiles += 1;
            bytes += md.map_or(0, |m| m.len());
        }
        if !name.starts_with('.') {
            names.push((name, is_dir));
        }
    }
    names.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| crate::apps::norm_key(&a.0).cmp(&crate::apps::norm_key(&b.0))));
    names.truncate(DIR_NAMES);
    Body::Dir { dirs, files: nfiles, bytes, names, more }
}

/// The preview of `path`, with a picture scaled to fit `bw` x `bh`.
pub fn of(path: &str, bw: u32, bh: u32) -> Preview {
    let name = path.rsplit('/').find(|s| !s.is_empty()).unwrap_or("/").to_string();
    let lmd = std::fs::symlink_metadata(path).ok();
    let md = std::fs::metadata(path).ok().or_else(|| lmd.clone());
    let link = lmd
        .as_ref()
        .filter(|m| m.file_type().is_symlink())
        .and_then(|_| std::fs::read_link(path).ok())
        .map(|t| t.to_string_lossy().into_owned());
    let is_dir = md.as_ref().is_some_and(|m| m.is_dir());
    let size = md.as_ref().map_or(0, |m| m.len());
    let mut p = Preview {
        path: path.to_string(),
        kind: files::kind_of(&name),
        name,
        is_dir,
        size,
        mtime: md.as_ref().map(|m| m.mtime()),
        mode: md.as_ref().map(|m| perms(m.permissions().mode())),
        link,
        body: Body::Unreadable,
    };
    if md.is_none() {
        return p;
    }
    p.body = if is_dir {
        folder(path)
    } else if let Some(b) = image(path, size, bw, bh) {
        b
    } else {
        use std::io::Read;
        let mut head = Vec::with_capacity(TEXT_BYTES);
        match std::fs::File::open(path).and_then(|f| f.take(TEXT_BYTES as u64 + 1).read_to_end(&mut head)) {
            Err(_) => Body::Unreadable,
            Ok(_) => {
                let cut = head.len() > TEXT_BYTES;
                head.truncate(TEXT_BYTES);
                if !head.is_empty() && is_text(&head) {
                    let (lines, more) = text_lines(&head, cut);
                    Body::Text { lines, more }
                } else {
                    Body::Plain
                }
            }
        }
    };
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> String {
        let d = std::env::temp_dir().join(format!("lunar-preview-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.to_string_lossy().into_owned()
    }

    #[test]
    fn permissions_read_the_way_ls_writes_them() {
        assert_eq!(perms(0o755), "rwxr-xr-x");
        assert_eq!(perms(0o640), "rw-r-----");
        assert_eq!(perms(0o100_644), "rw-r--r--");
    }

    #[test]
    fn a_text_is_text_even_when_the_read_cut_a_character() {
        assert!(is_text(b"hola\nmundo"));
        assert!(!is_text(b"\x7fELF\0\0"));
        let s = "año".as_bytes();
        assert!(is_text(&s[..2]), "a cut ñ is the read's");
        assert!(!is_text(b"\xff\xfeab"), "bad UTF-8 in the middle is not text");
    }

    #[test]
    fn the_lines_keep_their_shape_and_say_when_they_go_on() {
        let (l, more) = text_lines(b"a\tb\r\nc\n", false);
        assert_eq!(l, ["a    b", "c"]);
        assert!(!more);
        let long: String = (0..100).map(|i| format!("{i}\n")).collect();
        let (l, more) = text_lines(long.as_bytes(), false);
        assert_eq!(l.len(), TEXT_LINES);
        assert!(more);
    }

    #[test]
    fn each_kind_of_entry_gets_its_own_body() {
        let d = tmp("kinds");
        std::fs::write(format!("{d}/notas.txt"), "uno\ndos\n").unwrap();
        std::fs::write(format!("{d}/bin"), [0u8, 1, 2, 3]).unwrap();
        std::fs::create_dir(format!("{d}/sub")).unwrap();
        std::fs::write(format!("{d}/sub/a"), "x").unwrap();
        let mut pm = Pixmap::new(40, 20).unwrap();
        pm.fill(tiny_skia::Color::from_rgba8(255, 0, 0, 255));
        pm.save_png(format!("{d}/foto.png")).unwrap();

        assert!(matches!(of(&format!("{d}/notas.txt"), 100, 100).body, Body::Text { ref lines, .. } if lines == &["uno", "dos"]));
        assert!(matches!(of(&format!("{d}/bin"), 100, 100).body, Body::Plain));
        match of(&d, 100, 100).body {
            Body::Dir { dirs, files, names, .. } => {
                assert_eq!((dirs, files), (1, 3));
                assert_eq!(names[0], ("sub".to_string(), true), "folders first");
            }
            _ => panic!("a folder"),
        }
        match of(&format!("{d}/foto.png"), 20, 100).body {
            Body::Image { pm, w, h } => {
                assert_eq!((w, h), (40, 20));
                assert_eq!((pm.width(), pm.height()), (20, 10), "scaled to fit, aspect kept");
            }
            _ => panic!("a picture"),
        }
        assert!(matches!(of(&format!("{d}/nada"), 10, 10).body, Body::Unreadable));
        let _ = std::fs::remove_dir_all(&d);
    }
}
