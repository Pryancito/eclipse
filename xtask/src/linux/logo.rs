//! Eclipse's own logo in the image: the ASCII mark, the login banner and a
//! real icon for the icon theme.
//!
//! Three readers, three forms of the same drawing:
//!
//! - **The console.** `/etc/motd` is where Linux puts a login banner, and
//!   `/usr/share/eclipse/logo.txt` is the mark on its own, for anything that
//!   wants to draw it beside its own output (`fastfetch --logo-type file`,
//!   a splash, a bug-report template).
//! - **The icon theme.** `/etc/os-release`'s `LOGO` field names an icon, not a
//!   distribution: readers resolve `distributor-logo-<LOGO>` through the
//!   installed theme. Shipping `distributor-logo-eclipse.svg` is what makes
//!   `LOGO=eclipse` true; until this existed the only honest value was `linux`.
//! - **fastfetch.** It draws an ASCII logo of its own, chosen by `ID` and
//!   then `ID_LIKE` from `/etc/os-release`. There is no builtin for
//!   `eclipse` and `ID_LIKE` says `ubuntu debian`, so left alone fastfetch
//!   puts UBUNTU's circle of friends on an Eclipse box. `/etc/fastfetch/`
//!   points it at ours instead, as the last candidate in its own search
//!   order, so a user config still wins.
//! - **`/etc/profile`.** Nothing prints `/etc/motd` here: `login` would, and
//!   the serial console has no login (`console.shell=true` drops straight into
//!   busybox `sh`). So the profile prints it, once.
//!
//! The drawing is not a picture kept in a string literal: it is a formula,
//! sampled on whatever grid of characters it is given. Two things and no more:
//!
//! - a disc of radius [`DISC`] that covers the middle of the frame, which is
//!   the moon, and is dark because nothing is in front of it;
//! - outside it a corona whose brightness is
//!   `(BASE + LOBE * cos^2(angle - LOBE_ANGLE)) * exp(-distance / FALLOFF)`,
//!   so there is light all the way round the limb and two broad streamers
//!   facing each other, as a real eclipse has.
//!
//! The luminance then picks a character out of [`RAMP`] by how much ink it
//! has, which is the whole trick of ASCII art and the reason this needs no
//! colour: `%` reads bright against `.` in any terminal, in any palette, with
//! the escape sequences stripped. [`noise`] breaks the four levels up into
//! dots where the light is already faint, because four clean bands is what no
//! photograph of an eclipse has ever looked like.
//!
//! Because it is a formula, [`draw`] takes a size: the banner is 39 columns
//! and the same eclipse comes out of `draw(68, 34)` for anything with room
//! for it. Nothing here is measured off a bitmap, so nothing here has to be
//! redrawn by hand to change its size.
//!
//! **Seven-bit ASCII only**, and 39 columns by 19 rows. The serial console is
//! 80 wide at its narrowest and the boot console wraps without mercy, so a
//! mark that needs UTF-8 box characters or a wide terminal is a mark that
//! comes out as garbage exactly where it is most likely to be seen. The height
//! is the other half of that: this is printed on EVERY login, and a console is
//! 25 rows, so a taller drawing would push the previous screen away each time
//! anyone opens a shell.

use super::os_release;
use std::{fs, os::unix, path::Path};

use crate::variant::Variant;

/// The ramp: the characters in order of how much ink they have. A luminance
/// between 0 and 1 picks one, and that is the whole of ASCII art.
const RAMP: [u8; 4] = *b" .:%";

/// The occulting disc, in half-heights. Inside it there is nothing to see:
/// the moon is not dark in the drawing because it is drawn dark, it is dark
/// because nothing is in front of it.
const DISC: f64 = 0.78;

/// How fast the corona falls off outside the limb, in half-heights. A real
/// corona falls off faster than this; a terminal has four levels of grey.
const FALLOFF: f64 = 0.30;

/// The corona that is there all the way round the limb.
const BASE: f64 = 0.45;

/// What the two opposite streamers add on top of `BASE`, at their brightest.
/// `cos^2` of the angle, so they are broad and they face each other, which is
/// what the equatorial streamers of a real eclipse do.
const LOBE: f64 = 0.95;

/// Where the streamers point, in degrees, measured with y pointing DOWN as a
/// terminal counts rows. 45 puts them lower-right and upper-left.
const LOBE_ANGLE: f64 = 45.0;

/// How hard the dither bites, and it only bites where the light is already
/// faint: the sky breaks up into dots, the limb stays solid. Without this the
/// corona comes out in four clean bands, which no photograph of an eclipse has
/// ever looked like.
const DITHER: f64 = 0.65;

/// A character cell is about twice as tall as it is wide, so x counts half.
/// Leave this out and the eclipse is an ellipse.
const ASPECT: f64 = 0.5;

/// The seed of [`noise`]. Every value gives the same eclipse with different
/// speckle; this one was picked by looking at them.
const SEED: u32 = 3;

/// What is written under the drawing.
const CAPTION: &str = "E C L I P S E   O S";

/// Value noise: one number per cell, the same number every time.
///
/// Not `rand`: the image has to be reproducible, so the same tree has to give
/// the same `/etc/motd` byte for byte on every machine that builds it.
fn noise(column: usize, row: usize) -> f64 {
    let mut h = (column as u32)
        .wrapping_mul(374_761_393)
        .wrapping_add((row as u32).wrapping_mul(668_265_263))
        .wrapping_add(SEED.wrapping_mul(2_654_435_761));
    h ^= h >> 13;
    h = h.wrapping_mul(1_274_126_177);
    h ^= h >> 16;
    f64::from(h) / f64::from(u32::MAX)
}

/// The eclipse, at whatever size is asked for.
///
/// Two things and no drawing: a disc of radius [`DISC`] that hides everything
/// behind it, and outside it a corona whose brightness is
/// `(BASE + LOBE * cos^2(angle - LOBE_ANGLE)) * exp(-distance / FALLOFF)`.
/// That is why it still looks like an eclipse at 39 columns and at 68: the
/// picture is not a transcription of a bitmap, it is the formula sampled on
/// whatever grid of characters it is given.
fn draw(columns: usize, rows: usize) -> String {
    let (cx, cy) = ((columns - 1) as f64 / 2.0, (rows - 1) as f64 / 2.0);
    // Everything is in half-heights, so the drawing scales with the frame.
    let unit = (rows - 1) as f64 / 2.0;
    let limb = DISC * unit;
    let lobe = LOBE_ANGLE.to_radians();
    let mut out = String::new();
    for row in 0..rows {
        let mut line = String::new();
        for column in 0..columns {
            let x = (column as f64 - cx) * ASPECT;
            let y = row as f64 - cy;
            let radius = x.hypot(y);
            let mut light = 0.0;
            if radius > limb {
                let streamer = (y.atan2(x) - lobe).cos().powi(2);
                light = (BASE + LOBE * streamer) * (-(radius - limb) / (FALLOFF * unit)).exp();
                // The dither only bites where the light is already faint, so
                // the limb stays solid and the sky breaks up into dots.
                light *= 1.0 - DITHER * noise(column, row) * (1.0 - light.min(1.0));
            }
            let step = (light.clamp(0.0, 0.999) * RAMP.len() as f64) as usize;
            line.push(char::from(RAMP[step]));
        }
        // Trailing spaces are invisible and the terminal does not want them.
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// The mark, as it goes into `/usr/share/eclipse/logo.txt` and `/etc/motd`:
/// the eclipse, a blank line, and the name centred under it.
fn mark() -> String {
    let art = draw(MARK_COLUMNS, MARK_ROWS - 2);
    let pad = (MARK_COLUMNS - CAPTION.len()) / 2;
    format!("{art}\n{blank:pad$}{CAPTION}\n", blank = "")
}

/// The frame the mark is drawn in, which is what has to fit in a terminal.
const MARK_COLUMNS: usize = 39;

/// How many lines the mark is, which is what has to fit in a SCREEN.
///
/// Two of them are the blank line and the caption, so [`draw`] gets the
/// rest.
///
/// The banner is printed on every login and a console is 25 rows, so the
/// drawing plus the release line plus the prompt have to leave something of
/// the previous screen standing. Twenty is the ceiling the test holds it to.
const MARK_ROWS: usize = 19;

/// Where the mark lives as an asset, for anything that wants to draw it.
const LOGO_PATH: &str = "usr/share/eclipse/logo.txt";

/// The icon name `/etc/os-release`'s `LOGO` field resolves to, by the Icon
/// Theme Specification's `distributor-logo-<LOGO>` convention.
const ICON_STEM: &str = "distributor-logo-eclipse";

/// Where fastfetch looks for a system-wide config.
///
/// fastfetch tries `<dir>/fastfetch/config.jsonc` over a list of config
/// directories that runs `$XDG_CONFIG_HOME`, `~/.config/`, `~/`,
/// `$XDG_CONFIG_DIRS`, `/etc/xdg/` and then `/etc/`, and it stops at the
/// FIRST one that parses. So this is the fallback and not a lock: anything
/// the user puts in `~/.config/fastfetch/config.jsonc` still wins.
const FASTFETCH_CONFIG: &str = "etc/fastfetch/config.jsonc";

/// The mark again, with fastfetch's colour placeholder on it.
///
/// A second file and not the same one because `logo.txt` is the plain asset:
/// anything that `cat`s it gets the drawing, not a stray `$1`. Both come out
/// of [`mark`], so they cannot drift.
const FASTFETCH_LOGO: &str = "usr/share/eclipse/logo.fastfetch.txt";

/// What fastfetch is told `$1` means. The name and not `35`: fastfetch's
/// colour parser takes names, `@n` for xterm-256 and `#rrggbb`, and a bare
/// number is not one of them.
const FASTFETCH_COLOUR: &str = "magenta";

/// The mark with fastfetch's colour placeholder on the front.
///
/// One `$1` is enough for the whole drawing: fastfetch carries the colour
/// from one logo line to the next for every logo type but chafa.
fn fastfetch_logo() -> String {
    format!("$1{}", mark())
}

/// What goes in `/etc/fastfetch/config.jsonc`.
///
/// Without this fastfetch draws **Ubuntu's** logo on Eclipse. It picks a
/// builtin by `ID` from `/etc/os-release`, has no builtin for `eclipse`, and
/// falls through to `ID_LIKE`, which on x86_64 and aarch64 says `ubuntu
/// debian` because that is honestly where the userspace comes from. Shipping
/// our own `LOGO` icon did nothing for it: that field names an ICON for the
/// icon theme, and fastfetch's ASCII logo is a different thing entirely.
fn fastfetch_config() -> String {
    format!(
        "{{\n\
         \x20 \"$schema\": \"https://github.com/fastfetch-cli/fastfetch/raw/dev/doc/json_schema.json\",\n\
         \x20 \"logo\": {{\n\
         \x20\x20\x20 \"type\": \"file\",\n\
         \x20\x20\x20 \"source\": \"/{FASTFETCH_LOGO}\",\n\
         \x20\x20\x20 \"color\": {{ \"1\": \"{FASTFETCH_COLOUR}\" }}\n\
         \x20 }}\n\
         }}\n"
    )
}

/// The icon: the same formula, read as vectors instead of as characters.
///
/// The ASCII picks a character by how bright the corona is at that cell; here
/// the same brightness picks how long a radial stroke is. So the icon leans
/// the same way the banner does, and it leans that way because
/// [`LOBE_ANGLE`] says so in both -- not because someone drew two pictures
/// and kept them in step by hand.
///
/// `#aa00aa` is the classic ANSI magenta, which is what
/// [`os_release::ANSI_COLOR`] (`0;35`) asks a terminal for: the console and
/// the icon theme then show the same colour rather than two purples that
/// nearly match.
fn icon_svg() -> String {
    /// The limb, in the 48-unit box.
    const LIMB: f64 = 13.0;
    /// How far the longest streamer reaches past it.
    const REACH: f64 = 7.0;
    /// How many strokes go round. Enough to read as a corona at 48 pixels and
    /// at 16, which is where this is actually seen.
    const STROKES: u32 = 36;

    let lobe = LOBE_ANGLE.to_radians();
    let peak = BASE + LOBE;
    let strokes: String = (0..STROKES)
        .map(|n| {
            let th = f64::from(n) * std::f64::consts::TAU / f64::from(STROKES);
            // The same `BASE + LOBE * cos^2` the characters are chosen by.
            let light = (BASE + LOBE * (th - lobe).cos().powi(2)) / peak;
            let (x0, y0) = (24.0 + th.cos() * LIMB, 24.0 + th.sin() * LIMB);
            let far = LIMB + REACH * light;
            let (x1, y1) = (24.0 + th.cos() * far, 24.0 + th.sin() * far);
            format!(
                "\x20\x20\x20 <path stroke-width=\"2\"                  d=\"M{x0:.2} {y0:.2}L{x1:.2} {y1:.2}\"/>\n"
            )
        })
        .collect();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!-- Eclipse OS distributor logo. Generated by xtask; edit logo.rs. -->\n\
         <svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 48 48\" \
         width=\"48\" height=\"48\">\n\
         \x20 <title>Eclipse OS</title>\n\
         \x20 <g fill=\"none\" stroke=\"{COLOUR}\" stroke-linecap=\"round\">\n\
         \x20\x20\x20 <circle cx=\"24\" cy=\"24\" r=\"{LIMB}\" stroke-width=\"1.5\" \
         opacity=\"0.45\"/>\n\
         {strokes}\x20 </g>\n\
         </svg>\n",
        COLOUR = "#aa00aa",
    )
}

/// `index.theme` for `hicolor`, written only when the themes do not already
/// carry one.
///
/// Without it the directory is not a theme at all: GTK's icon lookup walks
/// `index.theme` to learn which subdirectories hold which sizes, and a
/// `scalable/apps` it was never told about is skipped, icon and all. The
/// `hicolor-icon-theme` package provides this file, so on the desktop variant
/// this is a no-op -- which is exactly why it is written only when absent.
const HICOLOR_INDEX: &str = "[Icon Theme]\n\
                             Name=Hicolor\n\
                             Comment=Fallback icon theme\n\
                             Hidden=true\n\
                             Directories=scalable/apps\n\
                             \n\
                             [scalable/apps]\n\
                             Size=48\n\
                             MinSize=8\n\
                             MaxSize=512\n\
                             Type=Scalable\n\
                             Context=Applications\n";

/// The `/etc/profile` stanza that paints the banner, appended by
/// `LinuxRootfs::write_profile`.
///
/// It goes LAST in the profile, after the terminal-size probe: that probe ends
/// with a `\033[H` that homes the graphic console's cursor, so anything
/// printed before it is written and then drawn over.
pub(super) fn profile_stanza() -> String {
    format!(
        "# Eclipse OS: the login banner, once per interactive session.\n\
         # `login` is what prints /etc/motd on a Linux box and the serial\n\
         # console has no login (console.shell=true drops into busybox sh),\n\
         # so this is the only thing that can show it. The guard is for a\n\
         # re-sourced profile and for nested shells that inherit it: without\n\
         # it, every `sh` inside the session repaints the logo.\n\
         case \"$-\" in\n\
         \x20 *i*)\n\
         \x20\x20\x20 if [ -z \"$ECLIPSE_MOTD_SHOWN\" ] && [ -r /etc/motd ]; then\n\
         \x20\x20\x20\x20\x20 export ECLIPSE_MOTD_SHOWN=1\n\
         \x20\x20\x20\x20\x20 printf '\\033[{colour}m'\n\
         \x20\x20\x20\x20\x20 cat /etc/motd\n\
         \x20\x20\x20\x20\x20 printf '\\033[0m'\n\
         \x20\x20\x20 fi\n\
         \x20\x20\x20 ;;\n\
         esac\n",
        colour = os_release::ANSI_COLOR,
    )
}

/// `/etc/motd`: the mark, then which Eclipse this is.
///
/// Plain text, no escape sequences: `login` cats this file verbatim, and a
/// motd full of escapes is a motd that reads as garbage the moment anything
/// but a terminal looks at it. The colour is applied by whoever prints it.
fn motd(version: &str, arch: &str, variant: Variant) -> String {
    format!(
        "{mark}\nEclipse OS {version} - {arch} {}\n",
        variant.name(),
        mark = mark()
    )
}

/// Installs the logo into `rootfs`: the asset, the banner and the icon.
pub(super) fn install(rootfs: &Path, arch: &str, variant: Variant) {
    let share = rootfs.join("usr/share/eclipse");
    fs::create_dir_all(&share).unwrap();
    let logo = rootfs.join(LOGO_PATH);
    fs::write(&logo, mark())
        .unwrap_or_else(|e| panic!("logo: cannot write {}: {e}", logo.display()));

    let version = os_release::eclipse_version();
    let etc = rootfs.join("etc");
    fs::create_dir_all(&etc).unwrap();
    // Replace, don't keep: the package tap's motd is Ubuntu's or Alpine's, and
    // this is the one line of the boot that says whose system this is.
    let motd_path = etc.join("motd");
    let _ = fs::remove_file(&motd_path);
    fs::write(&motd_path, motd(&version, arch, variant))
        .unwrap_or_else(|e| panic!("logo: cannot write {}: {e}", motd_path.display()));

    let apps = rootfs.join("usr/share/icons/hicolor/scalable/apps");
    fs::create_dir_all(&apps).unwrap();
    let icon = apps.join(format!("{ICON_STEM}.svg"));
    let _ = fs::remove_file(&icon);
    fs::write(&icon, icon_svg())
        .unwrap_or_else(|e| panic!("logo: cannot write {}: {e}", icon.display()));

    // The unsuffixed name, which is what a reader asking for "the distributor
    // logo" without consulting os-release looks up. A symlink and not a copy,
    // and only when nothing else claims the name: on the desktop variant the
    // package set may ship its own.
    let generic = apps.join("distributor-logo.svg");
    if !generic.exists() {
        let _ = unix::fs::symlink(format!("{ICON_STEM}.svg"), &generic);
    }

    let index = rootfs.join("usr/share/icons/hicolor/index.theme");
    if !index.exists() {
        let _ = fs::write(&index, HICOLOR_INDEX);
    }

    // And the one reader that draws an ASCII logo of its own. Left alone it
    // draws UBUNTU's, which is what `ID_LIKE` honestly says we come from.
    let ff_logo = rootfs.join(FASTFETCH_LOGO);
    fs::write(&ff_logo, fastfetch_logo())
        .unwrap_or_else(|e| panic!("logo: cannot write {}: {e}", ff_logo.display()));
    let ff_config = rootfs.join(FASTFETCH_CONFIG);
    fs::create_dir_all(ff_config.parent().unwrap()).unwrap();
    let _ = fs::remove_file(&ff_config);
    fs::write(&ff_config, fastfetch_config())
        .unwrap_or_else(|e| panic!("logo: cannot write {}: {e}", ff_config.display()));

    println!(
        "logo: {} ({MARK_COLUMNS}x{MARK_ROWS}), /etc/motd, {ICON_STEM}.svg and /{FASTFETCH_CONFIG}",
        LOGO_PATH
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{path::PathBuf, process::Command};

    /// How many DIFFERENT stroke lengths the icon has to have. One would mean
    /// a corona of equal rays, which is a sun.
    const STROKES_AT_LEAST: usize = 8;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "eclipse-logo-{tag}-{}-{}",
            std::process::id(),
            mark().len()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The constraint that decides what the drawing may be: seven-bit ASCII,
    /// and no wider than a narrow terminal. The boot console and the serial
    /// line both wrap without mercy, and a wrapped logo is worse than none.
    #[test]
    fn the_mark_is_ascii_and_fits_a_narrow_terminal() {
        let mark = mark();
        assert!(
            mark.is_ascii(),
            "the mark has non-ASCII bytes; the console font has no glyph for them"
        );
        assert!(
            !mark.contains('\t'),
            "a tab in the mark renders at whatever width the terminal feels like"
        );
        // `<=` and not `==`: the drawing is trimmed of trailing spaces, so
        // whether its widest line reaches the full frame is up to the
        // speckle, and the speckle is cosmetic. What matters is that it does
        // not go past the frame it was given.
        let widest = mark.lines().map(str::len).max().unwrap();
        assert!(
            widest <= MARK_COLUMNS,
            "the frame is {MARK_COLUMNS} columns and the drawing is {widest} wide"
        );
        // Asked of the measured width and not of `MARK_COLUMNS`: clippy folds
        // a comparison between two constants and rejects the assertion, and
        // the drawing is what has to fit anyway.
        assert!(
            widest <= 80,
            "{widest} columns does not fit the 80 of the serial console"
        );
        assert!(mark.ends_with('\n'), "the mark must end in a newline");

        // And it has to fit the SCREEN, not only the line: this is printed on
        // every login, and a drawing as tall as the console pushes the whole
        // previous screen away each time anyone opens a shell.
        let rows = mark.lines().count();
        assert_eq!(
            rows, MARK_ROWS,
            "MARK_ROWS says {MARK_ROWS} and the drawing is {rows}"
        );
        assert!(
            rows <= 20,
            "{rows} rows leaves nothing of a 25-row console standing"
        );
    }

    /// The dark hole in the middle, which is the one thing that makes this an
    /// eclipse and not a sun: the moon has to cover the middle of the frame.
    #[test]
    fn the_middle_of_the_eclipse_is_dark() {
        for (columns, rows) in [(39, 17), (68, 34), (24, 11)] {
            let drawing = draw(columns, rows);
            let art: Vec<&str> = drawing.lines().collect();
            let middle = art[rows / 2];
            let third = columns / 3;
            assert!(
                middle[third..columns - third].trim().is_empty(),
                "the disc is not covering the middle at {columns}x{rows}:\n{}",
                art.join("\n")
            );
        }
    }

    /// The disc is ROUND, which in a terminal is not the same as circular:
    /// a character cell is about twice as tall as it is wide, so the dark
    /// hole has to be about twice as wide in cells as it is tall. Drop
    /// [`ASPECT`] and the eclipse comes out as an egg standing on end.
    #[test]
    fn the_disc_is_round_on_a_terminal_and_not_an_egg() {
        let (columns, rows) = (68, 34);
        let art = padded(columns, rows);
        // Across the middle row and down the middle column, the run of blanks
        // is the disc: the corona is lit all the way round, so the run ends
        // where the limb is.
        let middle: String = art[rows / 2].iter().collect();
        let wide = longest_blank_run(&middle);
        let down: String = (0..rows).map(|r| art[r][columns / 2]).collect();
        let tall = longest_blank_run(&down);
        let ratio = wide as f64 / tall as f64;
        assert!(
            (1.6..=2.4).contains(&ratio),
            "the dark disc is {wide} wide and {tall} tall, ratio {ratio:.2}, \
             which is not round on a 1:2 cell"
        );
    }

    /// The drawing as a rectangle of characters, every line padded out, so a
    /// column can be read as easily as a row.
    fn padded(columns: usize, rows: usize) -> Vec<Vec<char>> {
        draw(columns, rows)
            .lines()
            .map(|l| {
                let mut v: Vec<char> = l.chars().collect();
                v.resize(columns, ' ');
                v
            })
            .collect()
    }

    /// The longest run of spaces inside a line, which is the disc.
    fn longest_blank_run(line: &str) -> usize {
        line.trim_end()
            .split(|c: char| c != ' ')
            .map(str::len)
            .max()
            .unwrap_or(0)
    }

    /// The corona is brightest at the limb and fades outwards, which is the
    /// whole of the formula. Walked along the streamer, where it is brightest.
    #[test]
    fn the_corona_fades_outwards_from_the_limb() {
        let (columns, rows) = (68, 34);
        let art = padded(columns, rows);
        let ink = |c: char| RAMP.iter().position(|r| char::from(*r) == c).unwrap();
        // Out along LOBE_ANGLE from the centre, in steps of one row.
        let (cx, cy) = ((columns - 1) as f64 / 2.0, (rows - 1) as f64 / 2.0);
        let (dx, dy) = (
            LOBE_ANGLE.to_radians().cos() / ASPECT,
            LOBE_ANGLE.to_radians().sin(),
        );
        let mut seen = Vec::new();
        for step in 0..rows {
            let (x, y) = (cx + dx * step as f64, cy + dy * step as f64);
            let (c, r) = (x.round() as usize, y.round() as usize);
            if r >= rows || c >= columns {
                break;
            }
            seen.push(ink(art[r][c]));
        }
        let peak = seen.iter().position(|v| *v == RAMP.len() - 1).expect(
            "nothing along the streamer reaches the brightest character; \
             the corona is not there",
        );
        assert!(
            seen[..peak].iter().all(|v| *v == 0),
            "there is light INSIDE the disc, along the streamer: {seen:?}"
        );
        let after = &seen[peak..];
        assert!(
            after.last() == Some(&0),
            "the corona never fades out: {seen:?}"
        );
        assert!(
            after.windows(2).all(|w| w[1] <= w[0] + 1),
            "the corona gets brighter again as it goes out: {seen:?}"
        );
    }

    /// There is corona ALL THE WAY ROUND the limb and not only where the two
    /// streamers are, which is what [`BASE`] is for: an eclipse with light on
    /// two sides and nothing on the others is a pair of brackets.
    #[test]
    fn the_limb_is_lit_all_the_way_round() {
        let (columns, rows) = (68, 34);
        let art = padded(columns, rows);
        let (cx, cy) = ((columns - 1) as f64 / 2.0, (rows - 1) as f64 / 2.0);
        let unit = (rows - 1) as f64 / 2.0;
        for degrees in (0..360).step_by(15) {
            let th = f64::from(degrees).to_radians();
            // Just outside the limb, in cells: x counts half, as it is drawn.
            let lit = (0..3).any(|out| {
                let r = DISC * unit + 0.5 + f64::from(out);
                let c = (cx + th.cos() * r / ASPECT).round();
                let y = (cy + th.sin() * r).round();
                if c < 0.0 || y < 0.0 || c as usize >= columns || y as usize >= rows {
                    return true; // off the frame: nothing to say about it
                }
                art[y as usize][c as usize] != ' '
            });
            assert!(lit, "the limb is dark at {degrees} degrees");
        }
    }

    /// The sky is DOTS and not four clean bands, which is what [`DITHER`] is
    /// for. Checked the way the dither shows: without it the formula is
    /// `cos^2`, which is the same turned half a circle, so the drawing would
    /// come out exactly symmetric under a 180-degree rotation. The speckle is
    /// what breaks that, and the bands are what it hides.
    #[test]
    fn the_sky_is_dotted_and_not_in_clean_bands() {
        let (columns, rows) = (68, 34);
        let art = padded(columns, rows);
        let turned: Vec<Vec<char>> = (0..rows)
            .map(|r| {
                (0..columns)
                    .map(|c| art[rows - 1 - r][columns - 1 - c])
                    .collect()
            })
            .collect();
        assert_ne!(
            art, turned,
            "the drawing is perfectly symmetric, so nothing is dithering it"
        );
    }

    /// The limb itself stays SOLID: the dither is weighted by how faint the
    /// light already is precisely so that it eats the sky and not the corona.
    /// A speckled limb reads as a dashed line, not as light.
    #[test]
    fn the_limb_itself_is_not_speckled() {
        let (columns, rows) = (68, 34);
        let art = padded(columns, rows);
        let (cx, cy) = ((columns - 1) as f64 / 2.0, (rows - 1) as f64 / 2.0);
        let unit = (rows - 1) as f64 / 2.0;
        let th = LOBE_ANGLE.to_radians();
        let brightest = char::from(RAMP[RAMP.len() - 1]);
        for out in 0..2 {
            let r = DISC * unit + 0.5 + f64::from(out);
            let c = (cx + th.cos() * r / ASPECT).round() as usize;
            let y = (cy + th.sin() * r).round() as usize;
            assert_eq!(
                art[y][c], brightest,
                "the limb is speckled at {out} cells out, on the streamer"
            );
        }
    }

    /// fastfetch draws an ASCII logo of its OWN, chosen by `ID` and then
    /// `ID_LIKE` from `/etc/os-release`. There is no builtin for `eclipse`
    /// and `ID_LIKE` says `ubuntu debian`, so without a config fastfetch
    /// puts Ubuntu's circle of friends on an Eclipse box. Shipping the
    /// `LOGO` icon did nothing for this: that field names an icon for the
    /// icon theme, which is a different reader.
    #[test]
    fn fastfetch_is_pointed_at_our_drawing_and_not_ubuntus() {
        let root = scratch("fastfetch");
        install(&root, "x86_64", Variant::Minimal);

        let config = root.join(FASTFETCH_CONFIG);
        let text =
            fs::read_to_string(&config).unwrap_or_else(|e| panic!("{}: {e}", config.display()));
        // The path as fastfetch will read it: absolute, and the file the
        // image actually installs.
        assert!(
            text.contains(&format!("\"source\": \"/{FASTFETCH_LOGO}\"")),
            "the config does not point at the logo we install:\n{text}"
        );
        assert!(
            root.join(FASTFETCH_LOGO).exists(),
            "the config points at a file the image does not install"
        );
        // `file` and not `file-raw`: raw means no placeholder replacement,
        // and the placeholder is the whole of how it gets its colour.
        assert!(
            text.contains("\"type\": \"file\""),
            "the logo type is not one that replaces the colour placeholder:\n{text}"
        );
    }

    /// The config has to PARSE, or fastfetch walks on to the next candidate
    /// and we are back to Ubuntu's logo with nothing said. Checked
    /// structurally, since xtask has no JSON parser: balanced braces outside
    /// strings, no trailing comma before a close, and quotes that close.
    #[test]
    fn the_fastfetch_config_is_well_formed() {
        let text = fastfetch_config();
        assert!(text.starts_with('{'), "not an object:\n{text}");
        assert!(text.ends_with("}\n"), "not closed:\n{text}");
        let mut depth = 0i32;
        let mut in_string = false;
        let mut previous = ' ';
        for (at, c) in text.char_indices() {
            match c {
                '"' if previous != '\\' => in_string = !in_string,
                '{' if !in_string => depth += 1,
                '}' if !in_string => {
                    depth -= 1;
                    assert!(depth >= 0, "a brace closes what was never opened:\n{text}");
                }
                ',' if !in_string => {
                    // A trailing comma is legal in JSONC and not in JSON, and
                    // this file is small enough to keep honest either way.
                    let next = text[at + 1..].trim_start().chars().next();
                    assert!(
                        !matches!(next, Some('}') | Some(']') | None),
                        "a trailing comma at byte {at}:\n{text}"
                    );
                }
                _ => {}
            }
            previous = c;
        }
        assert_eq!(depth, 0, "unbalanced braces:\n{text}");
        assert!(!in_string, "an unclosed string:\n{text}");
    }

    /// The logo fastfetch reads is the SAME drawing, with the placeholder in
    /// front of it and nothing else. Two files, one source.
    #[test]
    fn the_fastfetch_logo_is_the_same_drawing() {
        let drawn = fastfetch_logo();
        assert_eq!(
            drawn.strip_prefix("$1"),
            Some(mark().as_str()),
            "fastfetch's copy of the logo is not the logo"
        );
        // One placeholder is the point: fastfetch carries the colour from
        // line to line, so repeating it would be noise in a file people read.
        assert_eq!(drawn.matches("$1").count(), 1, "{drawn}");
        assert!(
            !mark().contains('$'),
            "the mark has a `$` in it, which fastfetch would eat as a placeholder"
        );
    }

    /// The console and fastfetch show the SAME colour. `/etc/profile` prints
    /// the motd in `os_release::ANSI_COLOR`, and fastfetch is told a name;
    /// this is what keeps the two from drifting into two different purples.
    #[test]
    fn fastfetch_is_given_the_colour_the_console_uses() {
        // `0;35` is ANSI magenta, and `magenta` is what fastfetch calls it.
        assert_eq!(os_release::ANSI_COLOR, "0;35");
        assert_eq!(FASTFETCH_COLOUR, "magenta");
        assert!(
            fastfetch_config().contains(&format!("\"1\": \"{FASTFETCH_COLOUR}\"")),
            "the config does not set the colour of the placeholder"
        );
    }

    /// The same tree has to give the same `/etc/motd`, byte for byte, on every
    /// machine that builds the image. That is why [`noise`] is a hash of the
    /// coordinates and not `rand`.
    #[test]
    fn the_drawing_is_reproducible() {
        assert_eq!(draw(39, 17), draw(39, 17), "the drawing is not stable");
        assert_eq!(mark(), mark(), "the mark is not stable");
    }

    /// It has to say whose it is: a ring of hashes alone is a ring of hashes.
    #[test]
    fn the_mark_carries_the_wordmark() {
        let mark = mark();
        let squashed: String = mark.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            squashed.contains("ECLIPSEOS"),
            "the mark no longer spells ECLIPSE OS:\n{mark}"
        );
    }

    /// `/etc/motd` is read by `login`, which cats it verbatim, so it stays
    /// plain. The colour belongs to whoever prints it.
    #[test]
    fn the_motd_is_plain_text_and_names_the_build() {
        let body = motd("1.2.3", "x86_64", Variant::Minimal);
        assert!(
            !body.contains('\u{1b}'),
            "the motd carries escape sequences:\n{body}"
        );
        assert!(body.contains(mark().trim_end()), "the motd lost the mark");
        assert!(body.contains("Eclipse OS 1.2.3"), "{body}");
        assert!(body.contains("x86_64"), "{body}");
        assert!(body.contains("minimal"), "{body}");
        let desktop = motd("1.2.3", "aarch64", Variant::Desktop);
        assert!(desktop.contains("aarch64 desktop"), "{desktop}");
    }

    /// The stanza runs in every interactive shell on the system, so a syntax
    /// error in it is a system where logging in prints an error before the
    /// prompt. Asked of a real `sh`, not read by eye.
    #[test]
    fn the_profile_stanza_is_valid_shell() {
        let dir = scratch("syntax");
        let f = dir.join("stanza.sh");
        fs::write(&f, profile_stanza()).unwrap();
        let out = Command::new("sh")
            .arg("-n")
            .arg(&f)
            .output()
            .expect("sh did not run");
        assert!(
            out.status.success(),
            "the stanza does not parse: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// And it prints the banner ONCE. Without the guard every nested `sh` in
    /// the session repaints the logo, which on a 25-row console means the
    /// previous command's output is gone.
    #[test]
    fn the_banner_is_printed_once_per_session_and_not_in_a_script() {
        let dir = scratch("once");
        let motd_file = dir.join("motd");
        fs::write(&motd_file, "LOGO-HERE\n").unwrap();
        // The stanza reads the absolute /etc/motd; point it at ours for the
        // test rather than writing into the host's /etc.
        let stanza = profile_stanza().replace("/etc/motd", motd_file.to_str().unwrap());
        let script = dir.join("profile");
        fs::write(&script, &stanza).unwrap();

        let run = |flags: &str| {
            let out = Command::new("sh")
                .arg("-c")
                .arg(format!(
                    "set {flags}; . {0}; . {0}; printf END",
                    script.display()
                ))
                .output()
                .expect("sh did not run");
            assert!(
                out.status.success(),
                "sh failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).to_string()
        };

        // Interactive: once, however many times the profile is sourced.
        let interactive = run("-i");
        assert_eq!(
            interactive.matches("LOGO-HERE").count(),
            1,
            "printed {} times:\n{interactive}",
            interactive.matches("LOGO-HERE").count()
        );
        // Non-interactive (a script, a service, `sh -c`): never. A banner in
        // the middle of a pipeline is corruption, not decoration.
        let batch = run("+i");
        assert!(
            !batch.contains("LOGO-HERE"),
            "a non-interactive shell painted the banner:\n{batch}"
        );
    }

    /// The icon is the whole reason `os-release` may say `LOGO=eclipse`, so
    /// the file has to land under the name the spec's convention builds.
    #[test]
    fn the_icon_lands_where_the_theme_looks_for_it() {
        let root = scratch("icon");
        install(&root, "x86_64", Variant::Minimal);

        let icon = root.join("usr/share/icons/hicolor/scalable/apps/distributor-logo-eclipse.svg");
        let svg = fs::read_to_string(&icon).unwrap_or_else(|e| panic!("{}: {e}", icon.display()));
        assert!(svg.starts_with("<?xml"), "{svg}");
        assert!(
            svg.contains("viewBox"),
            "a scalable icon needs a viewBox:\n{svg}"
        );
        assert!(svg.contains("</svg>"), "the svg is not closed:\n{svg}");
        assert!(
            svg.contains("Eclipse OS"),
            "the icon does not name us:\n{svg}"
        );
        // The same two pieces the ASCII draws: the whole limb...
        assert!(svg.contains("<circle"), "the icon lost the limb:\n{svg}");
        // ...and the corona over it, as radial strokes. Without them the icon
        // is a plain ring, which is a moon and not an eclipse -- and a ring is
        // exactly what dropping the strokes leaves behind, still well-formed.
        let strokes = svg.matches("<path").count();
        assert!(
            strokes >= 24,
            "the icon lost the corona; it is a ring, not an eclipse:\n{svg}"
        );
        // And the strokes are not all the same length, which is the whole of
        // what `LOBE` says: a corona of equal rays is a sun, not an eclipse.
        let lengths: std::collections::BTreeSet<&str> = svg
            .lines()
            .filter_map(|l| l.split('L').nth(1))
            .map(str::trim)
            .collect();
        assert!(
            lengths.len() > STROKES_AT_LEAST,
            "every corona stroke is the same length:\n{svg}"
        );

        // The unsuffixed name resolves to ours, as a link and not a copy.
        let generic = root.join("usr/share/icons/hicolor/scalable/apps/distributor-logo.svg");
        assert_eq!(
            fs::read_link(&generic).unwrap(),
            PathBuf::from("distributor-logo-eclipse.svg")
        );
        // And without index.theme the directory is not a theme: the icon
        // would be on disk and invisible to every GTK lookup.
        let index = fs::read_to_string(root.join("usr/share/icons/hicolor/index.theme")).unwrap();
        assert!(index.contains("[scalable/apps]"), "{index}");
    }

    /// The asset and the banner both land, and the asset is the bare mark.
    #[test]
    fn the_asset_is_the_mark_on_its_own() {
        let root = scratch("asset");
        install(&root, "riscv64", Variant::Minimal);
        assert_eq!(fs::read_to_string(root.join(LOGO_PATH)).unwrap(), mark());
        let motd = fs::read_to_string(root.join("etc/motd")).unwrap();
        assert!(motd.contains("riscv64"), "{motd}");
        // The asset carries no version, so it does not go stale.
        assert!(
            !mark().contains(char::is_numeric),
            "the asset names a version"
        );
    }

    /// An `index.theme` or a generic logo that something else put there is
    /// theirs: on the desktop variant the package set ships both, and the
    /// build must not fight it.
    #[test]
    fn what_the_packages_already_claim_is_left_alone() {
        let root = scratch("respect");
        let apps = root.join("usr/share/icons/hicolor/scalable/apps");
        fs::create_dir_all(&apps).unwrap();
        fs::write(apps.join("distributor-logo.svg"), "<!-- ubuntu -->").unwrap();
        fs::write(
            root.join("usr/share/icons/hicolor/index.theme"),
            "[Icon Theme]\nName=FromThePackage\n",
        )
        .unwrap();

        install(&root, "x86_64", Variant::Desktop);

        assert_eq!(
            fs::read_to_string(apps.join("distributor-logo.svg")).unwrap(),
            "<!-- ubuntu -->"
        );
        assert!(
            fs::read_to_string(root.join("usr/share/icons/hicolor/index.theme"))
                .unwrap()
                .contains("FromThePackage")
        );
        // Ours still lands under its own name.
        assert!(apps.join("distributor-logo-eclipse.svg").is_file());
    }

    /// An incremental `make image` runs this over a rootfs that already has
    /// it. Twice must be the same as once -- in particular not a link to a
    /// link, and not a motd with the mark in it twice.
    #[test]
    fn installing_twice_is_the_same_as_once() {
        let root = scratch("twice");
        install(&root, "x86_64", Variant::Minimal);
        let first = fs::read_to_string(root.join("etc/motd")).unwrap();
        install(&root, "x86_64", Variant::Minimal);
        let second = fs::read_to_string(root.join("etc/motd")).unwrap();
        assert_eq!(first, second);
        assert_eq!(second.matches("E C L I P S E").count(), 1, "{second}");
    }
}
