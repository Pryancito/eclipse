//! XDG icon lookup + raster cache for lunarbar's taskbar buttons and app
//! menu — dependency-light: tiny-skia's png-format feature decodes the PNGs
//! (pure Rust, the binary stays static), no librsvg/gdk-pixbuf. SVG/XPM-only
//! themes are skipped; any name without a PNG on disk falls back to the
//! letter badge the drawing layer provides.
//!
//! Resolution order for a name (an `Icon=` value or a Wayland app_id):
//! 1. an absolute path is loaded directly;
//! 2. the name (lowercased, `.png` suffix and reverse-DNS prefix stripped)
//!    against an index of every PNG under `$XDG_DATA_HOME/icons`,
//!    `$XDG_DATA_DIRS/*/icons` and `/usr/share/pixmaps`, preferring sizes
//!    near 48px and `apps/` categories — a pragmatic stand-in for full
//!    index.theme parsing that behaves identically on well-formed themes;
//! 3. the app_id against the `Icon=` of the .desktop file of the same stem
//!    (org.mozilla.firefox → firefox.desktop's icon), like real taskbars.
//!
//! Decoded icons are scaled once (bilinear, aspect preserved, centred) to the
//! requested slot size and cached; misses are cached too, so a window whose
//! app has no icon costs one lookup, not one per frame.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use tiny_skia::{FilterQuality, Pixmap, PixmapPaint, Transform};

/// Decoded (name, size) entries kept in RAM. A long session that opens many
/// distinct app_ids must not grow this without bound (amplifies heap pressure
/// that already stresses the kernel EventBus path on Eclipse).
const MAX_ICON_CACHE_ENTRIES: usize = 256;

/// Refuse to decode PNGs larger than this (raw file bytes) — protects the
/// event-loop thread from decompress bombs / multi‑MB wallpapers used as Icon=.
const MAX_PNG_BYTES: u64 = 4 * 1024 * 1024;

/// Refuse source pixmaps larger than this on either edge before scaling.
const MAX_PNG_DIM: u32 = 4096;

#[derive(Default)]
pub struct IconCache {
    /// Nested so a hit can look up by `&str` without allocating a `String` key.
    cache: HashMap<String, HashMap<u32, Option<Rc<Pixmap>>>>,
    /// Insertion order for approximate LRU eviction (front = oldest).
    order: VecDeque<(String, u32)>,
    /// Lowercased file stem → (score, path) of the best on-disk PNG.
    index: Option<HashMap<String, (i32, PathBuf)>>,
    /// Lowercased .desktop stem → its Icon= value.
    desktop: Option<HashMap<String, String>>,
}

impl IconCache {
    /// Entries currently held (hits and cached misses).
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.order.len()
    }

    /// Whether this exact (name, size) is still cached. Eviction has to drop
    /// the entry it chose and no other, and `len` cannot tell which one went.
    #[cfg(test)]
    pub(crate) fn holds(&self, name: &str, size: u32) -> bool {
        self.cache
            .get(name)
            .map(|by_size| by_size.contains_key(&size))
            .unwrap_or(false)
    }

    /// Distinct names cached. An eviction that empties a name's inner map has
    /// to drop the name with it, or a long session leaks one map per app_id.
    #[cfg(test)]
    pub(crate) fn names(&self) -> usize {
        self.cache.len()
    }

    /// Whether the two on-disk indexes have been built.
    #[cfg(test)]
    pub(crate) fn warm(&self) -> (bool, bool) {
        (self.index.is_some(), self.desktop.is_some())
    }

    /// Build the on-disk indexes up front. Both are built lazily on first
    /// lookup otherwise — and that first lookup happens inside a render pass,
    /// on the same thread that services pointer, keyboard and configure
    /// events, so a cold-cache walk of thousands of theme files would freeze
    /// the panel mid-interaction. Called once at startup, before the bars are
    /// mapped, where a pause costs nothing.
    pub fn prewarm(&mut self) {
        self.index.get_or_insert_with(build_index);
        self.desktop.get_or_insert_with(desktop_icon_map);
    }

    /// Resolve `name` to a `size`×`size` pixmap, or None (cached either way).
    pub fn get(&mut self, name: &str, size: u32) -> Option<Rc<Pixmap>> {
        let name = name.trim();
        if name.is_empty() || size == 0 {
            return None;
        }
        if let Some(by_size) = self.cache.get(name) {
            if let Some(hit) = by_size.get(&size) {
                return hit.clone();
            }
        }
        let loaded = self.load(name, size).map(Rc::new);
        while self.order.len() >= MAX_ICON_CACHE_ENTRIES {
            if let Some((old_n, old_s)) = self.order.pop_front() {
                if let Some(m) = self.cache.get_mut(&old_n) {
                    m.remove(&old_s);
                    if m.is_empty() {
                        self.cache.remove(&old_n);
                    }
                }
            } else {
                break;
            }
        }
        self.order.push_back((name.to_string(), size));
        self.cache
            .entry(name.to_string())
            .or_default()
            .insert(size, loaded.clone());
        loaded
    }

    fn load(&mut self, name: &str, size: u32) -> Option<Pixmap> {
        // 1) Absolute path straight from a .desktop Icon= value. Mutation
        //    reports the anchoring as removable, and for the answer it is:
        //    index keys are file stems, so they never contain a separator,
        //    and a relative path with one in it misses either way. It is
        //    anchored because this branch means "a path, not a name", and
        //    `theme/foo` is a name.
        if name.starts_with('/') {
            return load_scaled(Path::new(name), size);
        }
        // 2) The icon index, under the name's likely stems.
        let index = self.index.get_or_insert_with(build_index);
        for c in name_variants(name) {
            if let Some((_, p)) = index.get(&c) {
                if let Some(pm) = load_scaled(p, size) {
                    return Some(pm);
                }
            }
        }
        // 3) An app_id: follow the matching .desktop file's Icon= (one hop).
        let desktop = self.desktop.get_or_insert_with(desktop_icon_map);
        for c in name_variants(name) {
            if let Some(icon) = desktop.get(&c) {
                let icon = icon.clone();
                if icon.starts_with('/') {
                    return load_scaled(Path::new(&icon), size);
                }
                let index = self.index.get_or_insert_with(build_index);
                for ic in name_variants(&icon) {
                    if let Some((_, p)) = index.get(&ic) {
                        if let Some(pm) = load_scaled(p, size) {
                            return Some(pm);
                        }
                    }
                }
            }
        }
        None
    }
}

/// Lookup keys tried for a name: lowercased (sans .png), then the last
/// dot-segment (org.gnome.Calculator → calculator), the common convention
/// for reverse-DNS app_ids.
fn name_variants(name: &str) -> Vec<String> {
    let base = name.strip_suffix(".png").unwrap_or(name);
    let mut out = vec![base.to_ascii_lowercase()];
    if let Some(tail) = base.rsplit('.').next() {
        let tail = tail.to_ascii_lowercase();
        if !out.contains(&tail) {
            out.push(tail);
        }
    }
    out
}

/// XDG data roots (`$XDG_DATA_HOME`, then each of `$XDG_DATA_DIRS`).
fn data_roots() -> Vec<PathBuf> {
    data_roots_from(
        std::env::var("HOME").ok().as_deref(),
        std::env::var("XDG_DATA_HOME").ok().as_deref(),
        std::env::var("XDG_DATA_DIRS").ok().as_deref(),
    )
}

/// The same roots from the three variables' values, with the spec's defaults
/// applied: an unset or empty variable falls back the way the spec says, and
/// `$XDG_DATA_HOME` comes first so a user's theme shadows the system one.
/// Split from the read because the defaults are what matters here, and they
/// are exactly what a test cannot reach through the environment.
fn data_roots_from(
    home: Option<&str>,
    data_home: Option<&str>,
    data_dirs: Option<&str>,
) -> Vec<PathBuf> {
    let home = home.unwrap_or("/root");
    let data_home = data_home
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or_else(|| format!("{home}/.local/share"));
    let data_dirs = data_dirs
        .filter(|s| !s.is_empty())
        .unwrap_or("/usr/local/share:/usr/share");
    // The same list apps.rs walks for .desktop files, built by the same code.
    crate::apps::xdg_roots(&data_home, data_dirs)
}

/// Walk the icon directories once, keeping the best-scoring PNG per stem.
/// Iterative (explicit stack) instead of recursive so the coroutine call stack
/// stays shallow regardless of how deeply nested the theme tree is.
fn build_index() -> HashMap<String, (i32, PathBuf)> {
    index_from_roots(&data_roots())
}

/// The directories an icon index is built from: `<root>/icons` for each XDG
/// data root, then the legacy flat `/usr/share/pixmaps`, which is under no
/// root at all and is where a distro's unthemed PNGs still live.
fn index_dirs(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = roots.iter().map(|r| r.join("icons")).collect();
    dirs.push(PathBuf::from("/usr/share/pixmaps"));
    dirs
}

/// Index every PNG under `roots`' icon directories. Split from `build_index`
/// so the walk can be pointed at a tree of known shape.
fn index_from_roots(roots: &[PathBuf]) -> HashMap<String, (i32, PathBuf)> {
    let mut map = HashMap::new();
    // Bound the walk hard. Every entry visited is a path lookup in the
    // kernel, and `lookup_inode_at` is on the path implicated by
    // docs/README-crash-repro.md — so this runs once at startup and stays
    // small deliberately, trading exhaustive theme coverage (the letter
    // badge covers the misses) for a fraction of the filesystem traffic.
    let mut budget = 4_000usize;
    for root in index_dirs(roots) {
        walk(&root, &mut budget, &mut map);
    }
    map
}

fn walk(start: &Path, budget: &mut usize, map: &mut HashMap<String, (i32, PathBuf)>) {
    // Explicit stack instead of recursion: icon trees can be 8+ levels deep;
    // recursive calls would eat that many coroutine stack frames on each icon
    // index build, contributing to the kernel stack-overflow crash.
    let mut stack: Vec<(PathBuf, u32)> = vec![(start.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        // The budget half of this test is what mutation reports as redundant,
        // and for the index it is: the loop below stops on the same
        // condition, so a spent budget indexes nothing either way. It stays
        // because it is what makes the budget bound the STACK and not only
        // the files -- without it a symlinked cycle keeps popping
        // directories and reading them until the stack drains.
        if depth > 8 || *budget == 0 {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            if *budget == 0 {
                return;
            }
            *budget -= 1;
            let p = e.path();
            // is_dir() follows symlinks — icon themes commonly symlink whole size
            // directories; the walk budget bounds any symlink cycle.
            if p.is_dir() {
                // Cursor themes hold hundreds of files that are never app icons.
                if p.file_name().map(|n| n == "cursors").unwrap_or(false) {
                    continue;
                }
                stack.push((p, depth + 1));
            } else if p.extension().map(|x| x == "png").unwrap_or(false) {
                let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                let stem = stem.to_ascii_lowercase();
                let sc = score(&p);
                match map.get(&stem) {
                    Some((best, _)) if *best >= sc => {}
                    _ => {
                        map.insert(stem, (sc, p));
                    }
                }
            }
        }
    }
}

/// Prefer sizes near 48px (crisp when downscaled to bar size) and `apps/`
/// categories; unsized locations (pixmaps) sit in the middle.
fn score(p: &Path) -> i32 {
    let mut sc = 500;
    for comp in p.components() {
        let c = comp.as_os_str().to_string_lossy();
        if let Some((a, b)) = c.split_once('x') {
            if let (Ok(n), Ok(_)) = (a.parse::<i32>(), b.parse::<i32>()) {
                sc = 1000 - (n - 48).abs() * 4;
            }
        }
    }
    if p.to_string_lossy().contains("/apps") {
        sc += 50;
    }
    sc
}

/// Map every .desktop stem to its Icon= value (first hit per stem wins,
/// mirroring apps.rs's directory priority).
fn desktop_icon_map() -> HashMap<String, String> {
    desktop_map_from_roots(&data_roots())
}

/// The same map from an explicit root list, so the directory priority and the
/// parsing can be exercised against a tree of known shape.
fn desktop_map_from_roots(roots: &[PathBuf]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    // Same reasoning as index_from_roots: each entry is a path lookup, and
    // each .desktop file is an open+read+parse.
    let mut budget = 1_500usize;
    for root in roots {
        walk_desktop(&root.join("applications"), &mut budget, &mut map);
    }
    map
}

fn walk_desktop(start: &Path, budget: &mut usize, map: &mut HashMap<String, String>) {
    // Iterative to avoid deep call stacks inside the render path (same
    // rationale as `walk` above).
    let mut stack: Vec<(PathBuf, u32)> = vec![(start.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        if depth > 4 || *budget == 0 {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            if *budget == 0 {
                return;
            }
            *budget -= 1;
            let p = e.path();
            if p.is_dir() {
                stack.push((p, depth + 1));
            } else if p.extension().map(|x| x == "desktop").unwrap_or(false) {
                let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                let stem = stem.to_ascii_lowercase();
                if map.contains_key(&stem) {
                    continue;
                }
                if let Some(icon) = desktop_icon(&p) {
                    map.insert(stem, icon);
                }
            }
        }
    }
}

/// The `Icon=` value of a .desktop file's `[Desktop Entry]` group, if any.
fn desktop_icon(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > 256 * 1024 {
        return None;
    }
    let s = std::fs::read_to_string(path).ok()?;
    let mut in_entry = false;
    for line in s.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            if k.trim() == "Icon" {
                let v = v.trim();
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

/// Decode a PNG and scale it (bilinear, aspect preserved, centred) into a
/// `size`×`size` pixmap.
fn load_scaled(path: &Path, size: u32) -> Option<Pixmap> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > MAX_PNG_BYTES {
        return None;
    }
    let data = std::fs::read(path).ok()?;
    let src = Pixmap::decode_png(&data).ok()?;
    if src.width() > MAX_PNG_DIM || src.height() > MAX_PNG_DIM {
        return None;
    }
    let (w, h) = (src.width() as f32, src.height() as f32);
    // Unreachable through `decode_png`, which cannot hand back a pixmap of
    // zero on either edge -- so mutation is right that no PNG kills this. It
    // stays because the scale below divides by `w.max(h)`, and a decoder that
    // ever answered with an empty pixmap would make that a division by zero
    // and the transform NaN.
    if w < 1.0 || h < 1.0 {
        return None;
    }
    let mut dst = Pixmap::new(size, size)?;
    let s = size as f32 / w.max(h);
    let tx = (size as f32 - w * s) / 2.0;
    let ty = (size as f32 - h * s) / 2.0;
    let paint = PixmapPaint {
        quality: FilterQuality::Bilinear,
        ..PixmapPaint::default()
    };
    dst.draw_pixmap(
        0,
        0,
        src.as_ref(),
        &paint,
        Transform::from_row(s, 0.0, 0.0, s, tx, ty),
        None,
    );
    Some(dst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_cache_stays_bounded_under_unique_lookups() {
        let mut cache = IconCache::default();
        // Misses are cached too — without a ceiling this would grow forever
        // across a long session of distinct app_ids.
        for i in 0..(MAX_ICON_CACHE_ENTRIES * 3) {
            let _ = cache.get(&format!("no-such-icon-{i}"), 24);
        }
        assert!(
            cache.len() <= MAX_ICON_CACHE_ENTRIES,
            "icon cache must stay ≤ {}, got {}",
            MAX_ICON_CACHE_ENTRIES,
            cache.len()
        );
    }

    #[test]
    fn icon_cache_hit_does_not_grow() {
        let mut cache = IconCache::default();
        let _ = cache.get("missing-a", 24);
        let n = cache.len();
        for _ in 0..100 {
            let _ = cache.get("missing-a", 24);
        }
        assert_eq!(cache.len(), n);
    }

    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A throwaway directory tree under the system temp dir, removed when the
    /// test that built it ends. The walks take a path, so a tree of known
    /// shape is all they need -- no environment, no theme installed.
    struct Tree(PathBuf);

    impl Tree {
        fn new(tag: &str) -> Self {
            static N: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "lunarbar-icons-{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("temp dir");
            Tree(dir)
        }

        fn put(&self, rel: &str, bytes: &[u8]) -> PathBuf {
            let p = self.0.join(rel);
            std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
            std::fs::write(&p, bytes).expect("write");
            p
        }

        fn at(&self, rel: &str) -> PathBuf {
            self.0.join(rel)
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A real, opaque PNG, so the decoder has something to decode.
    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut pm = Pixmap::new(w, h).expect("pixmap");
        pm.fill(tiny_skia::Color::from_rgba8(0x20, 0x80, 0xc0, 0xff));
        pm.encode_png().expect("encode")
    }

    /// A PNG split down the middle, left red and right blue: two texels wide,
    /// so scaling it up either interpolates or does not.
    fn png_split() -> Vec<u8> {
        let mut pm = Pixmap::new(2, 1).expect("pixmap");
        let px = pm.pixels_mut();
        px[0] = tiny_skia::PremultipliedColorU8::from_rgba(0xff, 0, 0, 0xff).expect("red");
        px[1] = tiny_skia::PremultipliedColorU8::from_rgba(0, 0, 0xff, 0xff).expect("blue");
        pm.encode_png().expect("encode")
    }

    /// Every key a name can be looked up under, in the order they are tried:
    /// the name itself and, for a reverse-DNS app_id, its last segment. A name
    /// arrives as an `Icon=` value or as a Wayland app_id, in whatever case
    /// the application felt like.
    #[test]
    fn every_lookup_key_a_name_can_have_is_tried() {
        assert_eq!(name_variants("Firefox"), ["firefox"]);
        assert_eq!(name_variants("firefox.png"), ["firefox"]);
        assert_eq!(
            name_variants("org.mozilla.Firefox"),
            ["org.mozilla.firefox", "firefox"]
        );
        assert_eq!(
            name_variants("org.gnome.Calculator.png"),
            ["org.gnome.calculator", "calculator"]
        );
        // A name with no dot is one key, not the same key twice: the last
        // segment of a bare name is the name.
        assert_eq!(name_variants("gimp").len(), 1);
    }

    /// The score decides which of several PNGs with one name the panel draws.
    /// 48px is the target -- crisp when downscaled to the bar -- and an
    /// `apps/` category beats the same size anywhere else.
    #[test]
    fn an_icon_of_the_target_size_in_the_apps_category_outscores_the_rest() {
        let s = |p: &str| score(Path::new(p));
        assert_eq!(s("/usr/share/icons/T/48x48/apps/a.png"), 1050);
        assert_eq!(s("/usr/share/icons/T/48x48/mimetypes/a.png"), 1000);
        // Off the target costs four points a pixel, in either direction.
        assert_eq!(s("/usr/share/icons/T/16x16/apps/a.png"), 1000 - 32 * 4 + 50);
        assert_eq!(s("/usr/share/icons/T/64x64/apps/a.png"), 1000 - 16 * 4 + 50);
        // An unsized location sits in the middle: better than a 256px icon,
        // which downscales to mush, worse than one near the target.
        assert_eq!(s("/usr/share/pixmaps/a.png"), 500);
        assert!(s("/usr/share/icons/T/256x256/apps/a.png") < 500);
        // The bonus is for the category directory, not for a theme whose name
        // merely contains the word.
        assert_eq!(s("/usr/share/icons/myapps/48x48/x.png"), 1000);
        // The nominal size is the first number of the pair.
        assert_eq!(s("/i/16x48/x.png"), 1000 - 32 * 4);
        // A directory with an 'x' in it that is not a size changes nothing.
        assert_eq!(s("/usr/share/icons/oxygen/apps/a.png"), 550);
    }

    /// The index keeps the best-scoring PNG for each name, lowercased so an
    /// app_id finds it, and never the files that are not app icons.
    #[test]
    fn the_icon_index_keeps_the_best_png_for_each_name() {
        let t = Tree::new("index");
        t.put("a/48x48/apps/alpha.png", &png(48, 48));
        t.put("a/48x48/apps/MiXeD.png", &png(8, 8));
        t.put("a/48x48/apps/tie.png", &png(8, 8));
        t.put("a/cursors/left_ptr.png", &png(8, 8));
        t.put("a/scalable/apps/vector.svg", b"<svg/>");
        t.put("b/16x16/apps/alpha.png", &png(16, 16));
        t.put("b/48x48/apps/tie.png", &png(8, 8));
        let mut map = HashMap::new();
        let mut budget = 4_000usize;
        walk(&t.at("a"), &mut budget, &mut map);
        walk(&t.at("b"), &mut budget, &mut map);

        // The 48px copy wins over the 16px one whichever root it was in.
        assert_eq!(map["alpha"].1, t.at("a/48x48/apps/alpha.png"));
        // Two equal scores: the first root wins, which is the priority order
        // the roots were given in.
        assert_eq!(map["tie"].1, t.at("a/48x48/apps/tie.png"));
        // Keys are lowercased, so a lookup by app_id reaches them.
        assert!(map.contains_key("mixed"));
        assert!(!map.contains_key("MiXeD"));
        // A cursor theme is hundreds of files and none of them an app icon.
        assert!(!map.contains_key("left_ptr"));
        // PNG only: nothing in this binary decodes an SVG.
        assert!(!map.contains_key("vector"));
    }

    /// The walk is bounded in both directions it can run away in: a theme
    /// that symlinks a directory into itself, and one with more files than the
    /// panel is willing to stat before it maps.
    #[test]
    fn the_index_walk_stops_at_the_depth_and_the_budget_it_was_given() {
        let t = Tree::new("depth");
        t.put("r/48x48/apps/shallow.png", &png(8, 8));
        t.put("r/1/2/3/4/5/6/7/8/mid.png", &png(8, 8));
        t.put("r/1/2/3/4/5/6/7/8/9/deep.png", &png(8, 8));
        let mut map = HashMap::new();
        let mut budget = 4_000usize;
        walk(&t.at("r"), &mut budget, &mut map);
        assert!(map.contains_key("shallow"));
        // Eight levels under the root is the last one read.
        assert!(map.contains_key("mid"));
        assert!(!map.contains_key("deep"));
        // A budget already spent indexes nothing at all.
        let mut spent = 0usize;
        let mut none = HashMap::new();
        walk(&t.at("r"), &mut spent, &mut none);
        assert!(none.is_empty());
    }

    /// An icon directory is `<root>/icons`, for each data root in order, plus
    /// the flat legacy directory that is under no root at all.
    #[test]
    fn the_index_is_built_from_each_roots_icons_directory_and_the_legacy_one() {
        assert_eq!(
            index_dirs(&[PathBuf::from("/a"), PathBuf::from("/b")]),
            [
                PathBuf::from("/a/icons"),
                PathBuf::from("/b/icons"),
                PathBuf::from("/usr/share/pixmaps"),
            ]
        );

        let t = Tree::new("roots");
        t.put("one/icons/48x48/apps/lunar-one.png", &png(48, 48));
        t.put("two/icons/48x48/apps/lunar-two.png", &png(48, 48));
        // Not under `icons/`: a data root is not itself an icon directory.
        t.put("one/48x48/apps/lunar-bare.png", &png(48, 48));
        let map = index_from_roots(&[t.at("one"), t.at("two")]);
        assert!(map.contains_key("lunar-one"));
        assert!(map.contains_key("lunar-two"));
        assert!(!map.contains_key("lunar-bare"));
    }

    /// The defaults the XDG spec gives for the three variables, which is all
    /// this function is for: an unset or empty variable is not an empty root.
    #[test]
    fn an_unset_data_variable_falls_back_the_way_the_spec_says() {
        assert_eq!(
            data_roots_from(Some("/home/u"), None, None),
            [
                PathBuf::from("/home/u/.local/share"),
                PathBuf::from("/usr/local/share"),
                PathBuf::from("/usr/share"),
            ]
        );
        // Empty counts as unset, per the spec.
        assert_eq!(
            data_roots_from(Some("/home/u"), Some(""), Some("")),
            data_roots_from(Some("/home/u"), None, None)
        );
        // No HOME either: this image runs the panel as root.
        assert_eq!(
            data_roots_from(None, None, Some("/x")),
            [PathBuf::from("/root/.local/share"), PathBuf::from("/x")]
        );
        // Given values are used as given, user root first.
        assert_eq!(
            data_roots_from(Some("/h"), Some("/dh"), Some("/a:/b")),
            [
                PathBuf::from("/dh"),
                PathBuf::from("/a"),
                PathBuf::from("/b"),
            ]
        );
    }

    /// The `Icon=` of a .desktop file comes from its `[Desktop Entry]` group
    /// and from the plain key, not from an action's group or a localized
    /// spelling -- either of which would name a different icon.
    #[test]
    fn a_desktop_file_gives_up_the_icon_of_its_own_entry_group_only() {
        let t = Tree::new("desktop");
        let p = t.put(
            "x.desktop",
            b"[Desktop Action new]\nIcon=from-an-action\n\
              [Desktop Entry]\nName=X\nIcon[es]=localized\nIcon=right\n",
        );
        assert_eq!(desktop_icon(&p).as_deref(), Some("right"));
        // The value is trimmed; an empty value is not an icon.
        let p = t.put("y.desktop", b"[Desktop Entry]\nIcon=  spaced  \n");
        assert_eq!(desktop_icon(&p).as_deref(), Some("spaced"));
        let p = t.put("z.desktop", b"[Desktop Entry]\nIcon=\nName=Z\n");
        assert_eq!(desktop_icon(&p), None);
        // The key is everything before the FIRST '=': a path with one in it
        // is still a value.
        let p = t.put("v.desktop", b"[Desktop Entry]\nIcon=/opt/a=b/i.png\n");
        assert_eq!(desktop_icon(&p).as_deref(), Some("/opt/a=b/i.png"));
        // No Icon= at all, and a file that is not there.
        let p = t.put("w.desktop", b"[Desktop Entry]\nName=W\n");
        assert_eq!(desktop_icon(&p), None);
        assert_eq!(desktop_icon(&t.at("nope.desktop")), None);
        // A real .desktop file is longer than a line or two; only an absurd
        // one is refused unread.
        let mut big = b"[Desktop Entry]\nIcon=big\n".to_vec();
        big.extend_from_slice(&[b'#'; 4096]);
        big.push(b'\n');
        let p = t.put("big.desktop", &big);
        assert_eq!(desktop_icon(&p).as_deref(), Some("big"));
    }

    /// One hop from an app_id to an icon name: the .desktop file of the same
    /// stem, taken from the first root that has one, under `applications/`.
    #[test]
    fn the_desktop_map_takes_each_stem_from_the_first_root_that_has_it() {
        let t = Tree::new("dmap");
        t.put(
            "one/applications/shared.desktop",
            b"[Desktop Entry]\nIcon=from-one\n",
        );
        t.put(
            "two/applications/shared.desktop",
            b"[Desktop Entry]\nIcon=from-two\n",
        );
        t.put(
            "two/applications/sub/nested.desktop",
            b"[Desktop Entry]\nIcon=nested-icon\n",
        );
        t.put(
            "two/applications/MiXeD.desktop",
            b"[Desktop Entry]\nIcon=m\n",
        );
        // Not under `applications/`: a root is not itself the menu directory.
        t.put("one/loose.desktop", b"[Desktop Entry]\nIcon=loose-icon\n");
        let map = desktop_map_from_roots(&[t.at("one"), t.at("two")]);

        assert_eq!(map.get("shared").map(String::as_str), Some("from-one"));
        // A vendor subdirectory is walked, like apps.rs walks it.
        assert_eq!(map.get("nested").map(String::as_str), Some("nested-icon"));
        // Stems are lowercased, so an app_id reaches them.
        assert_eq!(map.get("mixed").map(String::as_str), Some("m"));
        assert!(!map.contains_key("loose"));
        // A spent budget maps nothing.
        let mut spent = 0usize;
        let mut none = HashMap::new();
        walk_desktop(&t.at("one/applications"), &mut spent, &mut none);
        assert!(none.is_empty());
    }

    /// A decoded icon fills the slot it was asked for, keeps its aspect and
    /// sits centred in it: a button's icon slot is square and a wide PNG
    /// stretched to fill it is what every other taskbar avoids doing.
    #[test]
    fn a_decoded_icon_is_square_and_centred_at_the_size_it_was_asked_for() {
        let t = Tree::new("scale");
        let lit = |pm: &Pixmap, x: u32, y: u32| pm.pixel(x, y).expect("pixel").alpha() > 0;

        // Wide: scaled by the long edge, so the blank bands are above and
        // below and the left column is painted.
        let p = t.put("wide.png", &png(40, 20));
        let pm = load_scaled(&p, 24).expect("wide");
        assert_eq!((pm.width(), pm.height()), (24, 24));
        assert!(lit(&pm, 12, 12), "nothing in the middle");
        assert!(!lit(&pm, 12, 0), "the top band is painted");
        assert!(!lit(&pm, 12, 23), "the bottom band is painted");
        assert!(lit(&pm, 0, 12), "not scaled to fit the long edge");

        // Tall: the same the other way round, which is what the centring is
        // for -- one of the two offsets is zero in either case.
        let p = t.put("tall.png", &png(20, 40));
        let pm = load_scaled(&p, 24).expect("tall");
        assert!(lit(&pm, 12, 12));
        assert!(!lit(&pm, 0, 12), "the left band is painted");
        assert!(!lit(&pm, 23, 12), "the right band is painted");
        assert!(lit(&pm, 12, 0), "not scaled to fit the long edge");
    }

    /// The scale is bilinear, the one quality choice in the whole path: an
    /// icon decoded at 48px and drawn at 24 is downscaled on every cold cache
    /// miss, and nearest-neighbour is what makes that look like 1998.
    #[test]
    fn an_icon_is_scaled_smoothly_and_not_by_nearest_neighbour() {
        let t = Tree::new("quality");
        let p = t.put("split.png", &png_split());
        let pm = load_scaled(&p, 32).expect("split");
        // Two texels blown up to 32 across: interpolation puts a gradient
        // between them, nearest-neighbour puts exactly two colours.
        let mut seen: Vec<(u8, u8)> = Vec::new();
        for x in 0..32 {
            let px = pm.pixel(x, 8).expect("pixel");
            let c = (px.red(), px.blue());
            if !seen.contains(&c) {
                seen.push(c);
            }
        }
        assert!(
            seen.len() > 2,
            "only {} distinct colours across",
            seen.len()
        );
    }

    /// The two ceilings that keep a decompress bomb out of the event loop: a
    /// multi-megabyte file is refused unread, and so is a pixmap too big on
    /// EITHER edge -- a 1x8000 strip is as much memory as a square one.
    #[test]
    fn an_icon_too_big_to_be_an_icon_is_refused() {
        let t = Tree::new("bombs");
        // Valid PNG data, then padding: the decoder would stop at IEND, so
        // what refuses this is the file size and nothing else.
        let mut padded = png(8, 8);
        padded.resize(MAX_PNG_BYTES as usize + 1, 0);
        let p = t.put("padded.png", &padded);
        assert!(load_scaled(&p, 24).is_none(), "a 4MiB icon was decoded");
        // One edge over the limit is over the limit.
        let p = t.put("strip.png", &png(MAX_PNG_DIM + 1, 4));
        assert!(load_scaled(&p, 24).is_none(), "a 4097px strip was decoded");
        let p = t.put("fine.png", &png(MAX_PNG_DIM, 4));
        assert!(load_scaled(&p, 24).is_some(), "a 4096px strip was refused");
        // Not a PNG at all, and not a file.
        let p = t.put("lies.png", b"GIF89a");
        assert!(load_scaled(&p, 24).is_none());
        assert!(load_scaled(&t.at("absent.png"), 24).is_none());
    }

    /// What the cache answers, and what it refuses to even look up. A zero
    /// slot and a blank name are both nothing to draw, and caching them would
    /// spend an entry on a question that has no answer.
    #[test]
    fn a_lookup_with_nothing_to_look_up_is_not_cached() {
        let mut c = IconCache::default();
        assert!(c.get("whatever", 0).is_none());
        assert!(c.get("", 24).is_none());
        assert!(c.get("   ", 24).is_none());
        assert_eq!(c.len(), 0, "an unanswerable lookup took an entry");
    }

    /// The name is trimmed before it is used as a key, so the same icon asked
    /// for with and without the spaces an `Icon=` line can carry is one entry
    /// and one lookup, not two.
    #[test]
    fn a_name_is_trimmed_before_it_becomes_a_cache_key() {
        let mut c = IconCache::default();
        let _ = c.get("  no-such-icon  ", 24);
        assert_eq!(c.len(), 1);
        let _ = c.get("no-such-icon", 24);
        assert_eq!(c.len(), 1, "the untrimmed name took its own entry");
        assert!(c.holds("no-such-icon", 24));
    }

    /// An absolute path is loaded as given -- that is what a .desktop file's
    /// `Icon=/opt/app/logo.png` means -- and the decoded pixmap is what the
    /// next frame gets back, at the size it asked for.
    #[test]
    fn an_absolute_path_is_loaded_as_given_and_then_remembered() {
        let t = Tree::new("abs");
        let p = t.put("logo.png", &png(32, 32));
        let name = p.to_str().expect("utf8");
        let mut c = IconCache::default();
        let first = c.get(name, 24).expect("decoded");
        assert_eq!((first.width(), first.height()), (24, 24));
        // The second ask is the cached one, and it is the icon, not a miss.
        let again = c.get(name, 24).expect("cached away");
        assert_eq!(c.len(), 1, "a hit took a second entry");
        assert!(Rc::ptr_eq(&first, &again), "decoded twice");
        // A different slot size is a different entry, at its own size.
        let bigger = c.get(name, 48).expect("decoded");
        assert_eq!(bigger.width(), 48, "the 24px entry answered for 48");
        assert_eq!(c.len(), 2);
        assert_eq!(c.names(), 1, "one name, two sizes");
    }

    /// Eviction drops the oldest entry and only that one: a long session
    /// opens many app_ids, and the ceiling is what keeps the heap off the
    /// kernel's EventBus path. The entry it chooses has to be the one it then
    /// forgets, and the name has to go with its last size.
    #[test]
    fn eviction_drops_the_oldest_entry_and_leaves_the_rest() {
        let mut c = IconCache::default();
        let _ = c.get("keep", 24);
        let _ = c.get("keep", 48);
        // One short of the ceiling, so the next insert evicts exactly once.
        for i in 0..(MAX_ICON_CACHE_ENTRIES - 2) {
            let _ = c.get(&format!("filler-{i}"), 24);
        }
        assert_eq!(c.len(), MAX_ICON_CACHE_ENTRIES);
        assert!(c.holds("keep", 24));
        let _ = c.get("one-too-many", 24);
        assert_eq!(c.len(), MAX_ICON_CACHE_ENTRIES);
        // The oldest went; its sibling size and the newcomer stayed.
        assert!(!c.holds("keep", 24), "the oldest entry survived");
        assert!(c.holds("keep", 48), "eviction took the whole name");
        assert!(c.holds("one-too-many", 24));
        assert!(c.holds("filler-0", 24), "eviction ran the wrong way round");
        // Evicting a name's last size drops the name too, or a long session
        // leaves one empty map per app_id behind.
        let _ = c.get("one-more", 24);
        assert!(!c.holds("keep", 48));
        assert_eq!(c.names(), c.len(), "an emptied name stayed behind");
    }

    /// Both indexes are built up front. They are built lazily otherwise, and
    /// that first lookup happens inside a render pass, on the thread that
    /// services pointer and keyboard events.
    #[test]
    fn prewarming_builds_both_of_the_on_disk_indexes() {
        let mut c = IconCache::default();
        assert_eq!(c.warm(), (false, false));
        c.prewarm();
        assert_eq!(c.warm(), (true, true), "an index was left cold");
    }
}
