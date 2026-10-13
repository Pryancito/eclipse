//! lunarclip — Eclipse OS's clipboard, and its history.
//!
//! Wayland has no clipboard the way X11 has one. The selection is *a client
//! offering data*: when the application that copied exits, the selection dies
//! with it, and nothing in the session remembers what it said. Copy a URL in
//! Firefox, close Firefox, paste — nothing. That is not a bug in labwc, it is
//! the protocol, and the answer everywhere else is a clipboard manager: one
//! client that watches the selection, keeps what went through it, and can put
//! an old entry back.
//!
//! The protocol for that is `zwlr_data_control_unstable_v1`, which labwc
//! implements through wlroots (see `--check`). It is the one protocol that lets
//! a client read and set the selection **without holding the keyboard focus**,
//! which is what separates a clipboard manager from a window that steals focus
//! on every copy.
//!
//! Alpine's answer to this is `wl-clipboard` + `clipman` + `wofi`: three
//! packages, one of them a Go program and one a GTK launcher that wants a
//! session D-Bus Eclipse does not have. This is the same thing in the shape the
//! rest of this crate already is — one static musl binary, no GTK, no D-Bus,
//! no Go — and with the pieces that matter here: it survives the application
//! that copied, and its picker is the same overlay as `lunarrun` and
//! `lunarfile`, so Super+V looks like the rest of the desktop.
//!
//! Modes:
//! - `--watch`: the daemon. Started by eclipse-init (`lunarclip.service`), it
//!   records every text selection and, when the source application exits,
//!   re-offers the last one so the clipboard outlives it. It also answers the
//!   socket the other modes talk to.
//! - `--pick`: the Super+V overlay. Type to filter, Enter puts the entry back
//!   on the clipboard, Esc closes. Needs the daemon.
//! - `--copy [TEXT]`: `wl-copy`. With no argument it reads stdin.
//! - `--paste`: `wl-paste`. Prints the selection on stdout.
//! - `--list` / `--clear`: the history on stdout, one line per entry, and
//!   forgetting it. `--list` is the thing to run when the picker looks wrong.
//! - `--check`: say whether this compositor has the protocol at all, and exit
//!   non-zero when it does not. The first thing to run on a session where the
//!   clipboard does not work.
//! - `--dump PATH:WxH`: render the picker to a raw ARGB8888 file and exit, with
//!   no compositor, for verification where there is no GPU and no session.
//!
//! Env: `ECLIPSE_LOOK=kde|eclipse|win11` overrides `/etc/eclipse/look`.

use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

use lunarbar::clip::{self, History, Req, ReqParse};
use lunarbar::draw::{Canvas, Rgb, GLYPH_H, GLYPH_W};
use lunarbar::fill_guard;
use lunarbar::keys::{
    key_char, BTN_LEFT, KEY_BACKSPACE_WL, KEY_DOWN_WL, KEY_ENTER_WL, KEY_ESC_WL, KEY_KPENTER_WL,
    KEY_PGDN_WL, KEY_PGUP_WL, KEY_TAB_WL, KEY_UP_WL, WHEEL_NOTCH,
};
use lunarbar::list::{inside_rect, move_selection, row_at_geom, Step};
use lunarbar::look::Look;
use lunarbar::proc::{self, map_shm_pool, BUFFERS};
use wayland_client::{
    protocol::{
        wl_buffer, wl_compositor, wl_keyboard, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm,
        wl_shm_pool, wl_surface,
    },
    Connection, Dispatch, Proxy, QueueHandle, WEnum,
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{self, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};

/// The MIME types this program offers when it is the one holding the selection.
///
/// All of them are the same bytes. The X11 atom names are in the list because
/// Xwayland asks for those: without `UTF8_STRING` a paste into `xterm` or into
/// an X11 Firefox gets nothing, and "the clipboard works everywhere except in
/// X11 applications" is the kind of bug that takes a week to pin down.
const OFFER_MIMES: &[&str] = &[
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
];

/// How long a selection read may take before it is given up on, in
/// milliseconds.
///
/// The bytes come from the application that copied, over a pipe, and nothing
/// obliges it to write them: a wedged Firefox that never answers `send` would
/// otherwise park the daemon forever on a blocking read, and from then on the
/// clipboard would be dead for the whole session with no message anywhere. Two
/// seconds is far more than a local pipe needs and short enough that the user
/// sees a slow copy rather than a hung desktop.
const READ_TIMEOUT_MS: i32 = 2000;

/// The same for a write, while this program is the one serving the selection.
const WRITE_TIMEOUT_MS: i32 = 2000;

/// How many clients the daemon's socket will hold at once.
///
/// One picker and a couple of `--copy` calls is the real number; the cap is
/// here so a client that opens connections and never writes cannot make the
/// daemon's poll set grow without end.
const MAX_CLIENTS: usize = 8;

/// The biggest request the daemon will buffer from one client before dropping
/// it: the per-entry cap plus room for the framing.
const MAX_REQ_BYTES: usize = clip::MAX_ENTRY_BYTES + 64;

// ── the command line ─────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Watch,
    Pick,
    Copy,
    Paste,
    List,
    Clear,
    Check,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Cli {
    mode: Mode,
    /// `--copy TEXT`: the text, or None to read stdin.
    text: Option<String>,
    /// `--dump PATH:WxH`, which renders the picker and exits.
    dump: Option<String>,
    /// `--no-persist`: do not re-offer the last entry when the application
    /// that copied exits. The escape hatch for a session where re-offering
    /// turns out to fight with something else.
    no_persist: bool,
    /// `--trim`: drop leading and trailing whitespace from what is copied,
    /// which is what a `--copy "$(cmd)"` of a command's output usually wants.
    trim: bool,
}

impl Default for Cli {
    fn default() -> Self {
        Self {
            mode: Mode::Pick,
            text: None,
            dump: None,
            no_persist: false,
            trim: false,
        }
    }
}

/// Parse the command line. `Err` carries what was wrong, for the usage text.
///
/// An iterator and not `env::args`, like every other tool in this crate: this
/// is the whole of the argument handling, and a function that reads the
/// process's own arguments can only be tested by running the process.
fn parse_args_from(args: impl Iterator<Item = String>) -> Result<Cli, String> {
    let mut cli = Cli::default();
    let mut mode: Option<Mode> = None;
    let mut words: Vec<String> = Vec::new();
    let mut args = args.peekable();
    // One mode, not the last one that happened to be typed: `--watch --paste`
    // is a mistake in a service file or a keybind, and starting the daemon
    // *and* printing the selection is not a reading of it anybody wants.
    let set_mode = |m: Mode, mode: &mut Option<Mode>| -> Result<(), String> {
        match mode {
            Some(prev) if *prev != m => Err("solo un modo a la vez".to_string()),
            _ => {
                *mode = Some(m);
                Ok(())
            }
        }
    };
    while let Some(a) = args.next() {
        match a.as_str() {
            "--watch" | "--daemon" => set_mode(Mode::Watch, &mut mode)?,
            "--pick" => set_mode(Mode::Pick, &mut mode)?,
            "--copy" => set_mode(Mode::Copy, &mut mode)?,
            "--paste" => set_mode(Mode::Paste, &mut mode)?,
            "--list" => set_mode(Mode::List, &mut mode)?,
            "--clear" => set_mode(Mode::Clear, &mut mode)?,
            "--check" => set_mode(Mode::Check, &mut mode)?,
            "--no-persist" => cli.no_persist = true,
            "--trim" => cli.trim = true,
            // A flag that takes a value never swallows the next FLAG as its
            // value: `--dump --trim` is a forgotten path, and writing a frame
            // to a file called `--trim` helps nobody.
            "--dump" => match args.peek() {
                Some(v) if !v.starts_with("--") => cli.dump = args.next(),
                _ => return Err("--dump necesita RUTA[:ANCHOxALTO]".to_string()),
            },
            "-h" | "--help" => return Err("help".to_string()),
            other if other.starts_with('-') => return Err(format!("opcion desconocida: {other}")),
            // Bare words are the text of a `--copy`, so `lunarclip --copy hola
            // mundo` works like `wl-copy hola mundo` does.
            other => words.push(other.to_string()),
        }
    }
    // The picker is the default, `--dump` included: a `--dump` with no mode is
    // a render of the picker, which is the only mode that draws anything.
    cli.mode = mode.unwrap_or(Mode::Pick);
    if !words.is_empty() {
        if cli.mode != Mode::Copy {
            return Err(format!("{} no toma argumentos sueltos", words.join(" ")));
        }
        cli.text = Some(words.join(" "));
    }
    Ok(cli)
}

fn usage() -> ! {
    eprintln!(
        "uso: lunarclip [MODO] [opciones]\n\
         \n\
         modos:\n\
         \x20 --pick            selector del historial (por defecto, Super+V)\n\
         \x20 --watch           demonio: vigila la seleccion y guarda el historial\n\
         \x20 --copy [TEXTO]    copia TEXTO (o la entrada estandar)\n\
         \x20 --paste           escribe la seleccion en la salida estandar\n\
         \x20 --list            escribe el historial, una linea por entrada\n\
         \x20 --clear           olvida el historial\n\
         \x20 --check           dice si el compositor trae wlr-data-control\n\
         \n\
         opciones:\n\
         \x20 --trim            quita los blancos de los extremos al copiar\n\
         \x20 --no-persist      no reofrecer la seleccion cuando muere quien copio\n\
         \x20 --dump RUTA:WxH   dibuja el selector en un ARGB8888 y sale"
    );
    std::process::exit(2)
}

// ── reading and writing a selection ──────────────────────────────────────────

/// A pipe, as `receive`/`send` need one.
fn pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0i32; 2];
    // O_CLOEXEC on both ends: this process spawns nothing today, but a pipe
    // leaking into a child is the kind of thing that makes a read hang on EOF
    // that never comes.
    let r = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    if r != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// Wait for `events` on `fd`, up to `timeout_ms`. True when it is ready.
fn wait_fd(fd: i32, events: i16, timeout_ms: i32) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    loop {
        let n = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return false;
        }
        return n > 0;
    }
}

/// Read everything on `fd`, with a timeout and a cap.
///
/// The cap is not the entry cap: a selection larger than the history keeps is
/// still read, because the point of the read is to find out what it says, and
/// `worth_keeping` is what decides whether it is stored. The cap here is only
/// a wall against a source that writes without end.
fn read_all(fd: OwnedFd, limit: usize) -> std::io::Result<Vec<u8>> {
    let raw = fd.as_raw_fd();
    let mut out = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    loop {
        if !wait_fd(raw, libc::POLLIN, READ_TIMEOUT_MS) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "la fuente no contesto",
            ));
        }
        let n = unsafe { libc::read(raw, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if n == 0 {
            return Ok(out);
        }
        out.extend_from_slice(&buf[..n as usize]);
        if out.len() > limit {
            out.truncate(limit);
            return Ok(out);
        }
    }
}

/// Write `text` to `fd` and close it, without blocking forever on a reader
/// that stopped reading.
///
/// A pipe holds 64 KiB; a paste of a 200 KiB entry into an application that
/// reads it slowly (or reads the first line and leaves) blocks after the first
/// buffer. Blocking there would freeze the daemon, so the write is bounded and
/// gives up — a truncated paste beats a dead clipboard, and the usual reason
/// for the second case is a reader that already has what it wanted.
fn write_all_timeout(fd: OwnedFd, text: &[u8]) {
    let raw = fd.as_raw_fd();
    let mut sent = 0usize;
    while sent < text.len() {
        if !wait_fd(raw, libc::POLLOUT, WRITE_TIMEOUT_MS) {
            return;
        }
        let n = unsafe {
            libc::write(
                raw,
                text[sent..].as_ptr() as *const libc::c_void,
                text.len() - sent,
            )
        };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            // EPIPE: the reader went away mid-paste. Nothing to report.
            return;
        }
        if n == 0 {
            return;
        }
        sent += n as usize;
    }
}

// ── the data-control client ──────────────────────────────────────────────────

/// Everything this program needs from the compositor to see and set the
/// selection. No surfaces: a clipboard manager draws nothing.
struct Clip {
    seat: Option<wl_seat::WlSeat>,
    manager: Option<ZwlrDataControlManagerV1>,
    manager_version: u32,
    device: Option<ZwlrDataControlDeviceV1>,
    /// The MIME types of each offer the compositor has announced but not yet
    /// made the selection.
    offers: Vec<(ZwlrDataControlOfferV1, Vec<String>)>,
    /// The selection as it stands, and what it offers.
    current: Option<(ZwlrDataControlOfferV1, Vec<String>)>,
    /// The source this program is serving, when it holds the selection, and
    /// the bytes it serves.
    source: Option<ZwlrDataControlSourceV1>,
    serving: Option<String>,
    /// A `selection` event that is the echo of this program's own
    /// `set_selection` and must NOT be read back.
    ///
    /// Reading it would deadlock: the read is synchronous, the bytes can only
    /// come from this process's own `send` handler, and that handler cannot run
    /// while the read is blocking the thread it would run on. Hence a flag set
    /// at the moment of setting and cleared by the echo.
    expect_own: bool,
    /// What the watcher has recorded, newest first.
    history: History,
    /// Where the history is written.
    path: PathBuf,
    /// Re-offer the last entry when the selection goes away.
    persist: bool,
    /// True once a selection has been seen, whatever it was: `--paste` cannot
    /// tell "the compositor has not told us yet" from "there is nothing on the
    /// clipboard" any other way.
    saw_selection: bool,
    /// Set when the compositor takes the device away (another manager with
    /// exclusive access, or the seat going). There is nothing to do but exit.
    finished: bool,
}

impl Clip {
    fn new(path: PathBuf, persist: bool) -> Self {
        Self {
            seat: None,
            manager: None,
            manager_version: 0,
            device: None,
            offers: Vec::new(),
            current: None,
            source: None,
            serving: None,
            expect_own: false,
            history: History::new(),
            path,
            persist,
            saw_selection: false,
            finished: false,
        }
    }

    fn mimes_of(&self, offer: &ZwlrDataControlOfferV1) -> Vec<String> {
        self.offers
            .iter()
            .find(|(o, _)| o == offer)
            .map(|(_, m)| m.clone())
            .unwrap_or_default()
    }

    /// Read the selection's text, or None when it carries none.
    fn read_selection(&mut self) -> Option<String> {
        let (offer, mimes) = self.current.clone()?;
        if clip::is_secret(&mimes) {
            return None;
        }
        let mime = clip::best_text_mime(&mimes)?;
        let (r, w) = pipe().ok()?;
        offer.receive(mime, w.as_fd());
        // The `receive` has to reach the compositor before the write end is
        // dropped here, or the source is handed a pipe nobody is waiting on.
        // The flush is the caller's job through `roundtrip`; dropping `w`
        // after it is what makes the read see EOF when the source is done.
        drop(w);
        let bytes = read_all(r, 8 * clip::MAX_ENTRY_BYTES).ok()?;
        String::from_utf8(bytes).ok()
    }

    /// Take the selection, serving `text` for as long as this process lives.
    fn serve(&mut self, text: String, qh: &QueueHandle<Clip>) {
        let (Some(mgr), Some(dev)) = (self.manager.clone(), self.device.clone()) else {
            return;
        };
        let src = mgr.create_data_source(qh, ());
        for m in OFFER_MIMES {
            src.offer((*m).to_string());
        }
        self.serving = Some(text);
        // Set the flag BEFORE the request: the echo can be dispatched as soon
        // as the next event is read, and a flag set afterwards would be set
        // after it was needed.
        self.expect_own = true;
        dev.set_selection(Some(&src));
        if let Some(old) = self.source.replace(src) {
            // The old source is cancelled by the compositor anyway; destroying
            // it here keeps one object per selection instead of one per copy
            // for the lifetime of the daemon.
            old.destroy();
        }
    }

    fn save(&self) {
        if let Err(e) = clip::save(&self.path, &self.history) {
            eprintln!("lunarclip: {}: {e}", self.path.display());
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for Clip {
    fn event(
        st: &mut Self,
        registry: &wl_registry::WlRegistry,
        ev: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = ev
        else {
            return;
        };
        match interface.as_str() {
            "zwlr_data_control_manager_v1" => {
                // Version 2 brings the primary selection; everything this
                // program does today is in version 1, so bind whatever is
                // offered and remember which it was for `--check`.
                let v = version.min(2);
                st.manager_version = v;
                st.manager = Some(registry.bind::<ZwlrDataControlManagerV1, _, _>(name, v, qh, ()));
            }
            // The FIRST seat only. A clipboard is per seat, and a session with
            // two seats is two sessions; binding both would record one user's
            // copies into the other's history.
            "wl_seat" if st.seat.is_none() => {
                st.seat =
                    Some(registry.bind::<wl_seat::WlSeat, _, _>(name, version.min(7), qh, ()));
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for Clip {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrDataControlManagerV1, ()> for Clip {
    fn event(
        _: &mut Self,
        _: &ZwlrDataControlManagerV1,
        _: <ZwlrDataControlManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for Clip {
    fn event(
        st: &mut Self,
        _: &ZwlrDataControlDeviceV1,
        ev: zwlr_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match ev {
            // A new offer, with its MIME types still to come. The offer is
            // bookkeeping until a `selection` names it.
            zwlr_data_control_device_v1::Event::DataOffer { id } => {
                fill_guard::try_push_bounded(&mut st.offers, (id, Vec::new()), MAX_OFFERS);
            }
            zwlr_data_control_device_v1::Event::Selection { id } => {
                let previous = st.current.take();
                match id {
                    Some(offer) => {
                        let mimes = st.mimes_of(&offer);
                        st.offers.retain(|(o, _)| *o != offer);
                        st.current = Some((offer, mimes));
                    }
                    None => st.current = None,
                }
                st.saw_selection = true;
                if let Some((o, _)) = previous {
                    o.destroy();
                }
            }
            // The compositor took the device away: another client asked for
            // exclusive data control, or the seat went. Nothing is recoverable
            // from here and a daemon that keeps running is a daemon that
            // silently stopped working.
            zwlr_data_control_device_v1::Event::Finished => st.finished = true,
            // The primary selection (middle-click paste) is not handled yet.
            // The offer still has to be destroyed, or every middle-click
            // leaks an object into a daemon that lives for the whole session.
            zwlr_data_control_device_v1::Event::PrimarySelection { id: Some(o) } => {
                st.offers.retain(|(x, _)| *x != o);
                o.destroy();
            }
            _ => {}
        }
    }

    wayland_client::event_created_child!(Clip, ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}

/// How many announced-but-unused offers are tracked. The compositor announces
/// one per selection change, and each is normally consumed by the `selection`
/// event right after it; the cap is a wall against a leak, not a budget.
const MAX_OFFERS: usize = 64;

impl Dispatch<ZwlrDataControlOfferV1, ()> for Clip {
    fn event(
        st: &mut Self,
        offer: &ZwlrDataControlOfferV1,
        ev: zwlr_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let zwlr_data_control_offer_v1::Event::Offer { mime_type } = ev else {
            return;
        };
        let mime_type = fill_guard::truncate_chars(&mime_type, 256);
        // An offer whose MIME types arrive after the `selection` that named it
        // is normal: the events are in order per object, not across objects.
        if let Some((_, mimes)) = st.offers.iter_mut().find(|(o, _)| o == offer) {
            fill_guard::try_push_bounded(mimes, mime_type, MAX_MIMES);
        } else if let Some((o, mimes)) = st.current.as_mut() {
            if o == offer {
                fill_guard::try_push_bounded(mimes, mime_type, MAX_MIMES);
            }
        }
    }
}

/// MIME types kept per offer. A `TARGETS` list from Xwayland is a dozen; a
/// LibreOffice copy is around thirty.
const MAX_MIMES: usize = 64;

impl Dispatch<ZwlrDataControlSourceV1, ()> for Clip {
    fn event(
        st: &mut Self,
        src: &ZwlrDataControlSourceV1,
        ev: zwlr_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match ev {
            // Somebody is pasting what this program holds.
            zwlr_data_control_source_v1::Event::Send { mime_type, fd } => {
                let _ = mime_type; // every MIME we offer is the same bytes
                if let Some(text) = st.serving.clone() {
                    write_all_timeout(fd, text.as_bytes());
                }
            }
            // Another client took the selection. Dropping the source here is
            // what keeps `serving` from answering for a selection this program
            // no longer holds.
            zwlr_data_control_source_v1::Event::Cancelled => {
                if st.source.as_ref() == Some(src) {
                    st.source = None;
                    st.serving = None;
                }
                src.destroy();
            }
            _ => {}
        }
    }
}

/// Connect, bind, and get the data device for the seat.
///
/// Returns the connection and queue alongside the state because every mode
/// needs all three and because a mode that got a state with no device must say
/// so with the same words as the others.
fn connect(
    path: PathBuf,
    persist: bool,
) -> Option<(Connection, wayland_client::EventQueue<Clip>, Clip)> {
    let conn = Connection::connect_to_env().ok()?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let mut st = Clip::new(path, persist);
    // Two roundtrips: the first delivers the globals, the second whatever the
    // binds emit.
    for _ in 0..2 {
        queue.roundtrip(&mut st).ok()?;
    }
    if let (Some(mgr), Some(seat)) = (st.manager.clone(), st.seat.clone()) {
        st.device = Some(mgr.get_data_device(&seat, &qh, ()));
        // A third roundtrip, so the device's first `selection` (the state of
        // the clipboard right now, which the compositor sends on bind) has
        // arrived before any mode looks at it.
        queue.roundtrip(&mut st).ok()?;
    }
    Some((conn, queue, st))
}

/// What a mode says when the session cannot do this at all.
fn no_protocol(st: &Clip) -> ! {
    if st.manager.is_none() {
        eprintln!(
            "lunarclip: este compositor no anuncia zwlr_data_control_manager_v1;\n\
             \x20          sin ese protocolo no hay portapapeles que vigilar (lunarclip --check)"
        );
    } else {
        eprintln!("lunarclip: no hay wl_seat; sin asiento no hay seleccion");
    }
    std::process::exit(1)
}

// ── the daemon ───────────────────────────────────────────────────────────────

/// A connected client of the daemon's socket.
struct Client {
    sock: UnixStream,
    buf: Vec<u8>,
}

/// Answer one request. Returns what to write back.
///
/// Split out from the loop so the whole of the daemon's behaviour on a request
/// is one function over the history, with the socket and the compositor left
/// to the caller: `pick` is the only thing that ever sends `SET`, and getting
/// its index wrong pastes the wrong entry.
fn answer(req: Req, st: &mut Clip, qh: &QueueHandle<Clip>) -> Vec<u8> {
    match req {
        Req::List => clip::encode_list(st.history.items()),
        Req::Set(i) => match st.history.promote(i) {
            Some(text) => {
                st.serve(text, qh);
                st.save();
                b"OK\n".to_vec()
            }
            None => b"ERR fuera de rango\n".to_vec(),
        },
        Req::Copy(text) => {
            if !clip::worth_keeping(&text) {
                return b"ERR nada que copiar\n".to_vec();
            }
            st.history.push(&text);
            st.serve(text, qh);
            st.save();
            b"OK\n".to_vec()
        }
        Req::Clear => {
            st.history.clear();
            st.save();
            b"OK\n".to_vec()
        }
    }
}

/// Bind the socket, replacing a stale one left by a daemon that was killed.
///
/// A unix socket is a file: it outlives the process that bound it, and
/// `eclipse-init` respawning this daemon would otherwise hit `EADDRINUSE`
/// forever. Connecting to it first is what tells a stale socket apart from a
/// daemon that is actually running — and when one is running, this one exits
/// rather than fighting it for the selection.
fn bind_socket(path: &std::path::Path) -> std::io::Result<UnixListener> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if path.exists() {
        if UnixStream::connect(path).is_ok() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                "ya hay un lunarclip --watch escuchando",
            ));
        }
        let _ = std::fs::remove_file(path);
    }
    UnixListener::bind(path)
}

/// The `--watch` loop: the compositor's events and the socket's clients.
fn watch(cli: &Cli) -> ! {
    let hist_path = clip::history_path(
        std::env::var("XDG_CACHE_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    );
    let Some((conn, mut queue, mut st)) = connect(hist_path.clone(), !cli.no_persist) else {
        eprintln!("lunarclip: no hay WAYLAND_DISPLAY (¿esta la sesion arriba?)");
        std::process::exit(1);
    };
    if st.device.is_none() {
        no_protocol(&st);
    }
    let qh = queue.handle();
    // The history from the last session, so Super+V after a reboot is not
    // empty. The entries are in the file; what is on the clipboard right now is
    // whatever the compositor says.
    st.history = clip::load(&hist_path);

    let sock_path = clip::socket_path(std::env::var("XDG_RUNTIME_DIR").ok().as_deref());
    let listener = match bind_socket(&sock_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("lunarclip: {}: {e}", sock_path.display());
            std::process::exit(1);
        }
    };
    let _ = listener.set_nonblocking(true);
    println!(
        "lunarclip: vigilando (socket {}, historial {} con {} entradas)",
        sock_path.display(),
        hist_path.display(),
        st.history.len()
    );

    let mut clients: Vec<Client> = Vec::new();
    loop {
        // Whatever the last dispatch left to do with the selection, before
        // sleeping: reading it, and re-offering it when it went away.
        settle_selection(&mut st, &qh, &conn, &mut queue);
        if st.finished {
            eprintln!("lunarclip: el compositor retiro el control de datos; saliendo");
            let _ = std::fs::remove_file(&sock_path);
            std::process::exit(1);
        }
        let _ = conn.flush();
        let Some(guard) = queue.prepare_read() else {
            if queue.dispatch_pending(&mut st).is_err() {
                break;
            }
            continue;
        };
        let wl_fd = guard.connection_fd().as_raw_fd();
        let mut pfds = vec![
            libc::pollfd {
                fd: wl_fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        for c in &clients {
            pfds.push(libc::pollfd {
                fd: c.sock.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        // No timeout: everything this daemon does is a reaction to an event,
        // so a tick would be a wakeup per second for the life of the session
        // with nothing to do in it.
        let ready = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, -1) };
        if ready < 0 {
            drop(guard);
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            eprintln!("lunarclip: poll: {err}");
            std::process::exit(1);
        }
        if pfds[0].revents & libc::POLLIN != 0 {
            let _ = guard.read();
        } else {
            drop(guard);
        }
        if queue.dispatch_pending(&mut st).is_err() {
            eprintln!("lunarclip: se perdio la conexion con el compositor");
            break;
        }
        // The clients the poll found ready, BY FILE DESCRIPTOR. The accept
        // below changes how many clients there are, so a position in `pfds` is
        // not a position in `clients` by the time it is used.
        let ready = ready_fds(&pfds, POLL_FIXED);
        if pfds[1].revents & libc::POLLIN != 0 {
            while let Ok((sock, _)) = listener.accept() {
                let _ = sock.set_nonblocking(true);
                if clients.len() >= MAX_CLIENTS {
                    // Dropping the newest and not the oldest: the oldest may be
                    // the picker the user is looking at.
                    continue;
                }
                clients.push(Client {
                    sock,
                    buf: Vec::new(),
                });
            }
        }
        // In reverse, so removing a client does not shift the index of one not
        // yet looked at. A client accepted just above is not in `ready` and is
        // read on the next turn of the loop: its bytes are waiting in the
        // socket, so the very next poll reports it readable with nothing lost.
        for i in (0..clients.len()).rev() {
            if !ready.contains(&clients[i].sock.as_raw_fd()) {
                continue;
            }
            if !serve_client(&mut clients[i], &mut st, &qh) {
                clients.remove(i);
            }
        }
    }
    let _ = std::fs::remove_file(&sock_path);
    std::process::exit(1)
}

/// The entries of the daemon's poll set that are not clients: the compositor's
/// connection and the listening socket, in that order.
const POLL_FIXED: usize = 2;

/// The descriptors `poll` reported something on, skipping the first `fixed`
/// entries.
///
/// By descriptor and not by position, which is the whole point. The accept loop
/// runs *after* the poll and pushes whatever connected, so by the time the
/// client list is walked it is longer than the poll set was — and indexing
/// `pfds[i + 2]` with the new length aborted the daemon on the first connection
/// anybody ever made to it ("len is 2 but the index is 2"). With
/// `panic = "abort"` that is not a message in a log, it is the clipboard gone
/// for the rest of the session, and eclipse-init respawning it into the same
/// crash on the next `lunarclip --check`.
///
/// Matching by descriptor is also immune to the other half of the problem: a
/// client removed mid-walk shifts every index after it, and nothing about that
/// is visible at the point where the index is used.
fn ready_fds(pfds: &[libc::pollfd], fixed: usize) -> Vec<i32> {
    pfds.iter()
        .skip(fixed)
        .filter(|p| p.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0)
        .map(|p| p.fd)
        .collect()
}

/// Read what a client sent, answer every complete request in it, and say
/// whether the connection is still worth keeping.
fn serve_client(c: &mut Client, st: &mut Clip, qh: &QueueHandle<Clip>) -> bool {
    let mut chunk = [0u8; 8192];
    loop {
        match c.sock.read(&mut chunk) {
            Ok(0) => return false, // the client closed
            Ok(n) => {
                c.buf.extend_from_slice(&chunk[..n]);
                if c.buf.len() > MAX_REQ_BYTES {
                    return false;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return false,
        }
    }
    loop {
        match clip::parse_req(&c.buf) {
            ReqParse::Done(req, used) => {
                c.buf.drain(..used);
                let out = answer(req, st, qh);
                if c.sock.write_all(&out).is_err() {
                    return false;
                }
            }
            ReqParse::Incomplete => return true,
            ReqParse::Bad => return false,
        }
    }
}

/// Do what the last batch of events implies: read a new selection into the
/// history, or put the last entry back when the selection went away.
///
/// This is deliberately not inside the `selection` event handler. Reading a
/// selection is a synchronous pipe read, and doing it from inside a dispatch
/// means doing it while the event queue is borrowed — the roundtrip that
/// flushes the `receive` request cannot happen there.
fn settle_selection(
    st: &mut Clip,
    qh: &QueueHandle<Clip>,
    conn: &Connection,
    queue: &mut wayland_client::EventQueue<Clip>,
) {
    if st.current.is_some() {
        if st.expect_own {
            // This is the echo of this program's own `set_selection`. Reading
            // it would deadlock on a pipe only this thread could fill.
            st.expect_own = false;
            if let Some((o, _)) = st.current.take() {
                o.destroy();
            }
            return;
        }
        // Flush the `receive` that `read_selection` is about to make: the
        // request has to reach the compositor, which then asks the source for
        // the bytes.
        if let Some(text) = {
            let out = st.read_selection();
            let _ = conn.flush();
            let _ = queue.roundtrip(st);
            out
        } {
            if st.history.push(&text) {
                st.save();
            }
        }
        if let Some((o, _)) = st.current.take() {
            o.destroy();
        }
        return;
    }
    // Nothing on the clipboard. Either nothing was ever copied, or — far more
    // often — whoever copied has exited, which on Wayland takes the clipboard
    // with it. Putting the last entry back is what makes a copy outlive the
    // window it came from, and it is the single most visible thing this daemon
    // does.
    if st.persist && st.saw_selection && st.source.is_none() {
        if let Some(text) = st.history.newest().map(str::to_string) {
            st.serve(text, qh);
            let _ = conn.flush();
        }
    }
}

// ── the one-shot modes ───────────────────────────────────────────────────────

/// Send one request to the daemon and read its answer, or None when there is no
/// daemon listening.
fn ask(req: &Req) -> Option<Vec<u8>> {
    let path = clip::socket_path(std::env::var("XDG_RUNTIME_DIR").ok().as_deref());
    let mut sock = UnixStream::connect(&path).ok()?;
    // A daemon that is wedged must not wedge the picker with it: both halves
    // are bounded so a keybind always comes back.
    let t = libc::timeval {
        tv_sec: 2,
        tv_usec: 0,
    };
    for opt in [libc::SO_RCVTIMEO, libc::SO_SNDTIMEO] {
        unsafe {
            libc::setsockopt(
                sock.as_raw_fd(),
                libc::SOL_SOCKET,
                opt,
                &t as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::timeval>() as libc::socklen_t,
            );
        }
    }
    sock.write_all(&clip::encode_req(req)).ok()?;
    let mut out = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match sock.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                out.extend_from_slice(&chunk[..n]);
                // Both answers are self-delimiting, so a reply is not waited
                // on until the daemon closes the socket (it does not).
                if clip::decode_list(&out).is_some()
                    || out.ends_with(b"\n") && out.starts_with(b"O")
                {
                    break;
                }
                if out.starts_with(b"ERR") && out.ends_with(b"\n") {
                    break;
                }
                if out.len() > 8 * clip::MAX_ENTRY_BYTES {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    Some(out)
}

/// The history, from the daemon if it is running and from its file otherwise.
///
/// The file is a fallback and not the source: the daemon has entries it has not
/// had a reason to write yet only in the moment between a copy and its save, but
/// a picker that shows nothing because the daemon is not running would leave the
/// user with no way to tell that apart from an empty history.
fn history_for_picker() -> (Vec<String>, bool) {
    if let Some(answer) = ask(&Req::List) {
        if let Some(items) = clip::decode_list(&answer) {
            return (items, true);
        }
    }
    let path = clip::history_path(
        std::env::var("XDG_CACHE_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    );
    (clip::load(&path).items().to_vec(), false)
}

/// `--copy`: hand the text to the daemon, or serve it here when there is none.
fn copy(cli: &Cli) -> ! {
    let text = match cli.text.clone() {
        Some(t) => t,
        None => {
            let mut buf = Vec::new();
            if let Err(e) = std::io::stdin().read_to_end(&mut buf) {
                eprintln!("lunarclip: no se pudo leer la entrada estandar: {e}");
                std::process::exit(1);
            }
            match String::from_utf8(buf) {
                Ok(t) => t,
                Err(_) => {
                    eprintln!("lunarclip: la entrada no es UTF-8 (solo texto, por ahora)");
                    std::process::exit(1);
                }
            }
        }
    };
    let text = if cli.trim {
        text.trim().to_string()
    } else {
        text
    };
    if text.is_empty() {
        eprintln!("lunarclip: nada que copiar");
        std::process::exit(1);
    }
    if let Some(answer) = ask(&Req::Copy(text.clone())) {
        if answer.starts_with(b"OK") {
            std::process::exit(0);
        }
        eprintln!(
            "lunarclip: el demonio dijo: {}",
            String::from_utf8_lossy(&answer).trim()
        );
        std::process::exit(1);
    }
    // No daemon: hold the selection here, in the foreground, for as long as
    // this process lives. That is how `wl-copy` works too (it forks), and it is
    // the only honest thing a clipboard owner can do on Wayland — the data
    // comes from the process that offered it. The message says so, because a
    // command that does not return is otherwise a command that hung.
    eprintln!(
        "lunarclip: no hay lunarclip --watch; sirvo la seleccion desde aqui\n\
         \x20          (Ctrl-C la suelta; con el demonio esto vuelve al momento)"
    );
    let Some((conn, mut queue, mut st)) = connect(PathBuf::new(), false) else {
        eprintln!("lunarclip: no hay WAYLAND_DISPLAY");
        std::process::exit(1);
    };
    if st.device.is_none() {
        no_protocol(&st);
    }
    let qh = queue.handle();
    st.serve(text, &qh);
    let _ = conn.flush();
    while !st.finished && st.source.is_some() {
        if queue.blocking_dispatch(&mut st).is_err() {
            break;
        }
    }
    std::process::exit(0)
}

/// `--paste`: the selection on stdout, with no trailing newline added.
fn paste() -> ! {
    let Some((conn, mut queue, mut st)) = connect(PathBuf::new(), false) else {
        eprintln!("lunarclip: no hay WAYLAND_DISPLAY");
        std::process::exit(1);
    };
    if st.device.is_none() {
        no_protocol(&st);
    }
    let text = st.read_selection();
    let _ = conn.flush();
    let _ = queue.roundtrip(&mut st);
    // The read above issued `receive` and then read the pipe; the roundtrip is
    // what gave the compositor the chance to ask the source for the bytes. On
    // a selection this program's own daemon serves, the first read can come
    // back empty because the source is another process that had not been asked
    // yet, so one retry after the roundtrip.
    let text = match text {
        Some(t) => Some(t),
        None => st.read_selection(),
    };
    match text {
        Some(t) => {
            let mut out = std::io::stdout();
            let _ = out.write_all(t.as_bytes());
            let _ = out.flush();
            std::process::exit(0)
        }
        None => {
            if st.saw_selection && st.current.is_none() {
                eprintln!("lunarclip: el portapapeles esta vacio");
            } else {
                eprintln!("lunarclip: la seleccion no ofrece texto");
            }
            std::process::exit(1)
        }
    }
}

/// `--check`: what this session actually has, and a non-zero exit when the
/// protocol is missing.
fn check() -> ! {
    let Some((_conn, _queue, st)) = connect(PathBuf::new(), false) else {
        println!("lunarclip: no hay WAYLAND_DISPLAY (¿esta la sesion arriba?)");
        std::process::exit(1);
    };
    let sock = clip::socket_path(std::env::var("XDG_RUNTIME_DIR").ok().as_deref());
    let daemon = UnixStream::connect(&sock).is_ok();
    println!(
        "zwlr_data_control_manager_v1: {}",
        match st.manager_version {
            0 => "NO ANUNCIADO".to_string(),
            v => format!(
                "v{v}{}",
                if v >= 2 {
                    " (con seleccion primaria)"
                } else {
                    ""
                }
            ),
        }
    );
    println!("wl_seat: {}", if st.seat.is_some() { "si" } else { "no" });
    println!(
        "demonio (--watch): {}",
        if daemon {
            format!("escuchando en {}", sock.display())
        } else {
            format!("no hay nada en {}", sock.display())
        }
    );
    let path = clip::history_path(
        std::env::var("XDG_CACHE_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    );
    println!(
        "historial: {} ({} entradas)",
        path.display(),
        clip::load(&path).len()
    );
    std::process::exit(i32::from(st.manager.is_none() || st.seat.is_none()))
}

/// `--list`: the history as one line per entry, newest first.
fn list() -> ! {
    let (items, from_daemon) = history_for_picker();
    if !from_daemon {
        eprintln!("lunarclip: sin demonio; leyendo el fichero del historial");
    }
    let mut out = std::io::stdout();
    for (i, item) in items.iter().enumerate() {
        let _ = writeln!(
            out,
            "{i:>3}  {:>9}  {}",
            clip::size_label(item.len()),
            clip::preview(item, 120)
        );
    }
    let _ = out.flush();
    std::process::exit(0)
}

/// `--clear`.
fn clear() -> ! {
    match ask(&Req::Clear) {
        Some(a) if a.starts_with(b"OK") => std::process::exit(0),
        Some(a) => {
            eprintln!(
                "lunarclip: el demonio dijo: {}",
                String::from_utf8_lossy(&a).trim()
            );
            std::process::exit(1)
        }
        None => {
            // No daemon to tell: the file is the whole history then.
            let path = clip::history_path(
                std::env::var("XDG_CACHE_HOME").ok().as_deref(),
                std::env::var("HOME").ok().as_deref(),
            );
            match clip::save(&path, &History::new()) {
                Ok(()) => std::process::exit(0),
                Err(e) => {
                    eprintln!("lunarclip: {}: {e}", path.display());
                    std::process::exit(1)
                }
            }
        }
    }
}

fn main() {
    // SIGPIPE: this program writes clipboard data into pipes whose reader can
    // leave at any moment (a paste into an application that reads one line and
    // closes). The default disposition is to die, which would take the daemon
    // down on a perfectly ordinary paste.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };

    let cli = match parse_args_from(std::env::args().skip(1)) {
        Ok(c) => c,
        Err(e) => {
            if e != "help" {
                eprintln!("lunarclip: {e}");
            }
            usage()
        }
    };

    // Offline render: no compositor, no seat, just a file to eyeball.
    if let Some(spec) = cli.dump.clone() {
        dump(&spec);
    }

    match cli.mode {
        Mode::Watch => watch(&cli),
        Mode::Copy => copy(&cli),
        Mode::Paste => paste(),
        Mode::Check => check(),
        Mode::List => list(),
        Mode::Clear => clear(),
        Mode::Pick => pick(),
    }
}

// ── the picker ───────────────────────────────────────────────────────────────

/// The colours of one look, the same shape `lunarfile` uses so the picker and
/// the file chooser are visibly the same desktop.
struct Palette {
    scrim: Rgb,
    scrim_a: f32,
    panel: Rgb,
    field: Rgb,
    text: Rgb,
    dim: Rgb,
    accent: Rgb,
    sel: Rgb,
    border: Rgb,
}

const PAL_KDE: Palette = Palette {
    scrim: (0, 0, 0),
    scrim_a: 0.45,
    panel: (0x2a, 0x2e, 0x32),
    field: (0x1f, 0x23, 0x26),
    text: (0xee, 0xf0, 0xf1),
    dim: (0x9a, 0xa0, 0xa6),
    accent: (0x3d, 0xae, 0xe9),
    sel: (0x31, 0x4a, 0x5e),
    border: (0x1b, 0x1e, 0x20),
};

const PAL_ECLIPSE: Palette = Palette {
    scrim: (0x0a, 0x08, 0x12),
    scrim_a: 0.5,
    panel: (0x1d, 0x19, 0x30),
    field: (0x15, 0x12, 0x24),
    text: (0xe0, 0xdc, 0xf4),
    dim: (0x9a, 0x92, 0xc0),
    accent: (0xb9, 0x8a, 0xe0),
    sel: (0x3a, 0x33, 0x57),
    border: (0x0f, 0x0d, 0x1a),
};

const PAL_WIN11: Palette = Palette {
    scrim: (0, 0, 0),
    scrim_a: 0.35,
    panel: (0x2b, 0x2b, 0x2b),
    field: (0x1f, 0x1f, 0x1f),
    text: (0xf0, 0xf0, 0xf0),
    dim: (0xa0, 0xa0, 0xa0),
    accent: (0x60, 0xa5, 0xfa),
    sel: (0x3a, 0x3a, 0x3a),
    border: (0x18, 0x18, 0x18),
};

fn palette(look: Look) -> &'static Palette {
    match look {
        Look::Kde => &PAL_KDE,
        Look::Eclipse => &PAL_ECLIPSE,
        Look::Win11 => &PAL_WIN11,
    }
}

const PANEL_W: i32 = 820;
const MIN_PANEL_W: i32 = 360;
const HEAD_H: i32 = 34;
const FIELD_H: i32 = 40;
const ROW_H: i32 = 30;
const PAD: i32 = 10;
const FOOT_H: i32 = 26;
/// At most this many rows on the screen at once; the rest is scrolled through.
const MAX_ROWS: usize = 12;
/// Characters of the filter kept. A filter longer than the field is a paste
/// into the wrong window.
const MAX_FIELD: usize = 128;
/// The size column, in characters: "999,9 KiB".
const SIZE_COL: i32 = 9;
const COL_GAP: i32 = 16;

/// Everything the picker draws and answers with.
struct Picker {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    layer_shell: Option<ZwlrLayerShellV1>,
    seat: Option<wl_seat::WlSeat>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,

    surface: Option<wl_surface::WlSurface>,
    layer: Option<ZwlrLayerSurfaceV1>,
    width: u32,
    height: u32,
    map: *mut u8,
    map_len: usize,
    buffers: [Option<wl_buffer::WlBuffer>; BUFFERS],
    busy: [bool; BUFFERS],
    next: usize,
    generation: u64,
    configured: bool,

    pal: &'static Palette,
    look: Look,
    /// The whole history, newest first. The index into THIS is what `SET`
    /// takes.
    items: Vec<String>,
    /// Whether the daemon answered. A picker with no daemon can show the file's
    /// entries but cannot put one back, and it says so instead of looking
    /// broken.
    live: bool,
    /// The filter.
    field: String,
    /// The visible rows: `(index into items, the preview)`.
    rows: Vec<(usize, String)>,
    sel: usize,
    top: usize,
    hover: Option<usize>,
    /// The last pointer position, which is what a click is tested against: a
    /// Button event carries no coordinates of its own.
    ptr: (f64, f64),
    panel: (i32, i32, i32, i32),
    row0_y: i32,
    rows_shown: usize,
    scroll_acc: f64,
    /// The index the user chose, once they have.
    picked: Option<usize>,
    done: bool,
}

impl Picker {
    fn new(items: Vec<String>, live: bool) -> Self {
        let look = Look::current();
        let mut p = Self {
            compositor: None,
            shm: None,
            layer_shell: None,
            seat: None,
            keyboard: None,
            pointer: None,
            surface: None,
            layer: None,
            width: 0,
            height: 0,
            map: std::ptr::null_mut(),
            map_len: 0,
            buffers: [None, None],
            busy: [false, false],
            next: 0,
            generation: 0,
            configured: false,
            pal: palette(look),
            look,
            items,
            live,
            field: String::new(),
            rows: Vec::new(),
            sel: 0,
            top: 0,
            hover: None,
            ptr: (-1.0, -1.0),
            panel: (0, 0, 0, 0),
            row0_y: 0,
            rows_shown: MAX_ROWS,
            scroll_acc: 0.0,
            picked: None,
            done: false,
        };
        p.refilter();
        p
    }

    /// Rebuild the rows from the filter, keeping the selection on the top row.
    ///
    /// Back to the top and not "nearest to where it was": the first row of a
    /// filtered list is the entry the user is looking for often enough that
    /// Enter right after typing has to be safe, and a selection left at row
    /// nine of a list that now has two rows is the bug that makes it unsafe.
    fn refilter(&mut self) {
        let needle = self.field.to_lowercase();
        self.rows = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, t)| needle.is_empty() || t.to_lowercase().contains(&needle))
            .map(|(i, t)| (i, clip::preview(t, 512)))
            .collect();
        self.sel = 0;
        self.top = 0;
    }

    fn move_sel(&mut self, delta: i32, step: Step) {
        let (sel, top) = move_selection(
            self.sel,
            self.top,
            self.rows.len(),
            delta,
            step,
            self.rows_shown.max(1),
        );
        self.sel = sel;
        self.top = top;
    }

    /// Choose the row at `row`, which ends the loop.
    fn choose(&mut self, row: usize) {
        if let Some((index, _)) = self.rows.get(row) {
            self.picked = Some(*index);
        }
        self.done = true;
    }

    fn map_overlay(&mut self, qh: &QueueHandle<Picker>) {
        let (Some(comp), Some(ls)) = (&self.compositor, &self.layer_shell) else {
            return;
        };
        if self.surface.is_some() {
            return;
        }
        let surface = comp.create_surface(qh, ());
        let layer = ls.get_layer_surface(
            &surface,
            None,
            zwlr_layer_shell_v1::Layer::Overlay,
            "clipboard".into(),
            qh,
            (),
        );
        layer.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
        layer.set_size(0, 0);
        // -1: the FULL output, panel included, so the scrim's clicks are this
        // surface's while it holds the keyboard.
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        surface.commit();
        self.surface = Some(surface);
        self.layer = Some(layer);
    }

    fn configure(&mut self, qh: &QueueHandle<Picker>, w: u32, h: u32) {
        let Some(shm) = self.shm.clone() else {
            return;
        };
        let Some((w, h)) = surface_size(w, h) else {
            eprintln!("lunarclip: {w}x{h} no es dibujable; nada hecho");
            return;
        };
        if self.configured && self.width == w && self.height == h {
            self.render();
            return;
        }
        let Some(geom) = proc::pool_geometry(w, h) else {
            eprintln!("lunarclip: {w}x{h} no cabe en un wl_shm pool");
            return;
        };
        let (total, stride, frame_size) = (geom.total, geom.stride, geom.frame_size);
        let Some((map, fd)) = map_shm_pool(total, "lunarclip") else {
            return;
        };
        self.generation += 1;
        let generation = self.generation;
        let pool = shm.create_pool(fd.as_fd(), total as i32, qh, ());
        let mk = |i: usize| {
            pool.create_buffer(
                (i * frame_size) as i32,
                w as i32,
                h as i32,
                stride as i32,
                wl_shm::Format::Argb8888,
                qh,
                (i, generation),
            )
        };
        let buffers = [Some(mk(0)), Some(mk(1))];
        pool.destroy();

        for b in self.buffers.iter_mut() {
            if let Some(b) = b.take() {
                b.destroy();
            }
        }
        if !self.map.is_null() && self.map_len > 0 {
            unsafe { libc::munmap(self.map as *mut libc::c_void, self.map_len) };
        }
        self.width = w;
        self.height = h;
        self.map = map;
        self.map_len = total;
        self.buffers = buffers;
        self.busy = [false, false];
        self.next = 0;
        self.configured = true;
        self.render();
    }

    fn render(&mut self) {
        if !self.configured || self.map.is_null() {
            return;
        }
        let Some(surface) = self.surface.clone() else {
            return;
        };
        let i = if !self.busy[self.next] {
            self.next
        } else if !self.busy[1 - self.next] {
            1 - self.next
        } else {
            return; // both held by the compositor; the next Release repaints
        };
        let (w, h) = (self.width as usize, self.height as usize);
        let frame_size = w * h * 4;
        let Some(mut cv) = Canvas::try_new(w, h) else {
            return;
        };
        let (panel, row0, fits) = draw_picker(&mut cv, w, h, self);
        self.panel = panel;
        self.row0_y = row0;
        self.rows_shown = fits;
        let data: &mut [u8] =
            unsafe { std::slice::from_raw_parts_mut(self.map.add(i * frame_size), frame_size) };
        if !cv.blit_argb(data) {
            return;
        }
        if let Some(b) = &self.buffers[i] {
            surface.attach(Some(b), 0, 0);
            surface.damage_buffer(0, 0, w as i32, h as i32);
            surface.commit();
            self.busy[i] = true;
            self.next = 1 - i;
        }
    }

    fn row_at(&self, x: f64, y: f64) -> Option<usize> {
        row_at_geom(
            self.panel,
            self.row0_y,
            ROW_H,
            self.rows_shown,
            self.top,
            self.rows.len(),
            x,
            y,
        )
    }

    fn inside_panel(&self, x: f64, y: f64) -> bool {
        inside_rect(self.panel, x, y)
    }
}

impl Drop for Picker {
    fn drop(&mut self) {
        for b in self.buffers.iter_mut() {
            if let Some(b) = b.take() {
                b.destroy();
            }
        }
        if !self.map.is_null() && self.map_len > 0 {
            unsafe { libc::munmap(self.map as *mut libc::c_void, self.map_len) };
        }
        if let Some(l) = self.layer.take() {
            l.destroy();
        }
        if let Some(s) = self.surface.take() {
            s.destroy();
        }
    }
}

/// The buffer size for a `configure` of `w` x `h`, or None for something this
/// client will not render. A zero means "you choose".
fn surface_size(w: u32, h: u32) -> Option<(u32, u32)> {
    let w = if w == 0 { 1280 } else { w };
    let h = if h == 0 { 720 } else { h };
    fill_guard::check_buffer(w as usize, h as usize).ok()?;
    Some((w, h))
}

/// The panel rect `(x, y, w, h)` for an output of `w` x `h` showing `rows`.
fn panel_rect(w: i32, h: i32, rows: usize) -> (i32, i32, i32, i32) {
    // Clamp to the output AFTER the minimum: on an output narrower than
    // MIN_PANEL_W a plain `min` would make the panel wider than the screen and
    // `x` negative. `clamp` is not used because its floor and ceiling cross
    // over in exactly that case, and it panics then.
    let roomy = PANEL_W.min(w - 96).max(MIN_PANEL_W);
    let pw = roomy.min(w.max(1));
    let shown = rows.clamp(1, MAX_ROWS) as i32;
    let want = PAD + HEAD_H + FIELD_H + PAD + shown * ROW_H + FOOT_H + PAD;
    let ph = want.min((h - 48).max(PAD * 2 + HEAD_H + FIELD_H + ROW_H + FOOT_H));
    let ph = ph.min(h.max(1));
    let px = (w - pw) / 2;
    let py = ((h - ph) / 2).max(0);
    (px, py, pw, ph)
}

/// How many rows actually fit in a panel of height `ph`.
fn rows_fitting(ph: i32) -> usize {
    let room = ph - (PAD + HEAD_H + FIELD_H + PAD + FOOT_H + PAD);
    if room < ROW_H {
        return 1;
    }
    ((room / ROW_H) as usize).min(MAX_ROWS)
}

/// How many characters of a preview fit beside the size column.
fn preview_chars(avail: i32) -> usize {
    let room = avail - (SIZE_COL * GLYPH_W) - COL_GAP;
    if room > 0 {
        ((room / GLYPH_W).max(1)) as usize
    } else {
        (((avail - 8) / GLYPH_W).max(1)) as usize
    }
}

/// Paint the scrim and the centred picker. Returns the panel rect, the y of the
/// first row and how many rows were drawn — the three things hit testing needs,
/// handed back together so it cannot be told one frame's geometry and another
/// frame's row count.
fn draw_picker(
    cv: &mut Canvas,
    w: usize,
    h: usize,
    st: &Picker,
) -> ((i32, i32, i32, i32), i32, usize) {
    let p = st.pal;
    cv.fill_rect_a(0, 0, w as i32, h as i32, p.scrim, p.scrim_a);

    let (px, py, pw, ph) = panel_rect(w as i32, h as i32, st.rows.len());
    cv.round_rect_a(px - 1, py - 1, pw + 2, ph + 2, 9, p.border, 0.9);
    let panel_a = if st.look == Look::Win11 { 0.92 } else { 0.97 };
    cv.round_rect_a(px, py, pw, ph, 8, p.panel, panel_a);

    // Header: what this is, and how many entries there are.
    let hx = px + PAD + 4;
    let hy = py + PAD + (HEAD_H - GLYPH_H) / 2;
    cv.text_bold("portapapeles", hx, hy, p.text);
    let count = if st.field.is_empty() {
        format!("{} entradas", st.items.len())
    } else {
        format!("{} de {}", st.rows.len(), st.items.len())
    };
    let cw = Canvas::text_width(&count);
    cv.text(&count, px + pw - PAD - 4 - cw, hy, p.dim);

    // The filter field.
    let fy = py + PAD + HEAD_H;
    cv.round_rect(px + PAD, fy, pw - 2 * PAD, FIELD_H - 6, 6, p.field);
    let ty = fy + (FIELD_H - 6 - GLYPH_H) / 2;
    if st.field.is_empty() {
        cv.text("filtrar...", px + PAD + 10, ty, p.dim);
    } else {
        let shown = clip::preview(&st.field, MAX_FIELD);
        let end = cv.text(&shown, px + PAD + 10, ty, p.text);
        // A caret, so an empty-looking field after a Backspace is visibly a
        // field with a cursor in it.
        cv.hline(end + 2, ty + GLYPH_H - 1, GLYPH_W, p.accent, 1.0);
    }

    let row0 = fy + FIELD_H + PAD - 6;
    let fits = rows_fitting(ph);
    let avail = pw - 2 * PAD - 16;

    if st.rows.is_empty() {
        let msg = if st.items.is_empty() {
            "el historial esta vacio"
        } else {
            "nada coincide"
        };
        cv.text(msg, px + PAD + 10, row0 + (ROW_H - GLYPH_H) / 2, p.dim);
    }

    for (n, (_, preview)) in st.rows.iter().skip(st.top).take(fits).enumerate() {
        let ry = row0 + (n as i32) * ROW_H;
        let absolute = st.top + n;
        if absolute == st.sel {
            cv.round_rect(px + PAD, ry, pw - 2 * PAD, ROW_H - 2, 5, p.sel);
            cv.active_line(px + PAD + 2, ry + 4, ROW_H - 10, p.accent);
        } else if st.hover == Some(absolute) {
            cv.round_rect_a(px + PAD, ry, pw - 2 * PAD, ROW_H - 2, 5, p.sel, 0.45);
        }
        let ly = ry + (ROW_H - 2 - GLYPH_H) / 2;
        let text = clip::preview(preview, preview_chars(avail));
        cv.text(&text, px + PAD + 10, ly, p.text);
        // The size on the right, from the ENTRY and not from the preview: the
        // point of the column is to say that a one-line-looking row is a
        // thousand lines.
        let bytes = st
            .rows
            .get(absolute)
            .and_then(|(i, _)| st.items.get(*i))
            .map(String::len)
            .unwrap_or(0);
        let label = clip::size_label(bytes);
        let lw = Canvas::text_width(&label);
        cv.text(&label, px + pw - PAD - 10 - lw, ly, p.dim);
    }

    // Footer: the keys, and the warning when there is no daemon to set the
    // selection — a picker that looks fine and pastes nothing is worse than one
    // that says why.
    let foot_y = py + ph - FOOT_H + (FOOT_H - GLYPH_H) / 2 - 2;
    if st.live {
        cv.text(
            "Enter copia  ^/v elige  Esc cierra",
            px + PAD + 4,
            foot_y,
            p.dim,
        );
    } else {
        cv.text(
            "sin lunarclip --watch: solo lectura",
            px + PAD + 4,
            foot_y,
            p.accent,
        );
    }
    ((px, py, pw, ph), row0, fits)
}

impl Dispatch<wl_registry::WlRegistry, ()> for Picker {
    fn event(
        st: &mut Self,
        registry: &wl_registry::WlRegistry,
        ev: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = ev
        else {
            return;
        };
        match interface.as_str() {
            "wl_compositor" => {
                st.compositor = Some(registry.bind::<wl_compositor::WlCompositor, _, _>(
                    name,
                    version.min(4),
                    qh,
                    (),
                ))
            }
            "wl_shm" => st.shm = Some(registry.bind::<wl_shm::WlShm, _, _>(name, 1, qh, ())),
            "zwlr_layer_shell_v1" => {
                st.layer_shell =
                    Some(registry.bind::<ZwlrLayerShellV1, _, _>(name, version.min(4), qh, ()))
            }
            "wl_seat" => {
                if st.seat.is_none() {
                    st.seat =
                        Some(registry.bind::<wl_seat::WlSeat, _, _>(name, version.min(7), qh, ()));
                }
            }
            "wl_output" => {
                // Bound and dropped: the overlay anchors to whatever output the
                // compositor gives it, but a client that never binds the
                // outputs gets no `configure` on some compositors.
                let _ = registry.bind::<wl_output::WlOutput, _, _>(name, version.min(3), qh, ());
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for Picker {
    fn event(
        _: &mut Self,
        _: &wl_output::WlOutput,
        _: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_compositor::WlCompositor, ()> for Picker {
    fn event(
        _: &mut Self,
        _: &wl_compositor::WlCompositor,
        _: wl_compositor::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm::WlShm, ()> for Picker {
    fn event(
        _: &mut Self,
        _: &wl_shm::WlShm,
        _: wl_shm::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for Picker {
    fn event(
        _: &mut Self,
        _: &wl_shm_pool::WlShmPool,
        _: wl_shm_pool::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for Picker {
    fn event(
        _: &mut Self,
        _: &wl_surface::WlSurface,
        _: wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrLayerShellV1, ()> for Picker {
    fn event(
        _: &mut Self,
        _: &ZwlrLayerShellV1,
        _: <ZwlrLayerShellV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for Picker {
    fn event(
        st: &mut Self,
        layer: &ZwlrLayerSurfaceV1,
        ev: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match ev {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                layer.ack_configure(serial);
                st.configure(qh, width, height);
            }
            zwlr_layer_surface_v1::Event::Closed => st.done = true,
            _ => {}
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, (usize, u64)> for Picker {
    fn event(
        st: &mut Self,
        _: &wl_buffer::WlBuffer,
        ev: wl_buffer::Event,
        &(i, generation): &(usize, u64),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if !matches!(ev, wl_buffer::Event::Release) {
            return;
        }
        // A release for a buffer from a previous size is not this frame's.
        if generation != st.generation || i >= BUFFERS {
            return;
        }
        st.busy[i] = false;
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for Picker {
    fn event(
        st: &mut Self,
        seat: &wl_seat::WlSeat,
        ev: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = ev
        else {
            return;
        };
        if caps.contains(wl_seat::Capability::Keyboard) && st.keyboard.is_none() {
            st.keyboard = Some(seat.get_keyboard(qh, ()));
        }
        if caps.contains(wl_seat::Capability::Pointer) && st.pointer.is_none() {
            st.pointer = Some(seat.get_pointer(qh, ()));
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for Picker {
    fn event(
        st: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        ev: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let wl_keyboard::Event::Key {
            key,
            state: WEnum::Value(wl_keyboard::KeyState::Pressed),
            ..
        } = ev
        else {
            return;
        };
        match key {
            KEY_ESC_WL => st.done = true,
            KEY_ENTER_WL | KEY_KPENTER_WL => {
                let sel = st.sel;
                st.choose(sel);
            }
            KEY_UP_WL => st.move_sel(-1, Step::Wrap),
            KEY_DOWN_WL | KEY_TAB_WL => st.move_sel(1, Step::Wrap),
            KEY_PGUP_WL => st.move_sel(-(st.rows_shown as i32), Step::Clamp),
            KEY_PGDN_WL => st.move_sel(st.rows_shown as i32, Step::Clamp),
            KEY_BACKSPACE_WL => {
                st.field.pop();
                st.refilter();
                st.render();
            }
            other => {
                if let Some(ch) = key_char(other) {
                    if st.field.chars().count() < MAX_FIELD {
                        st.field.push(ch);
                        st.refilter();
                    }
                    st.render();
                }
                return;
            }
        }
        if !st.done {
            st.render();
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for Picker {
    fn event(
        st: &mut Self,
        _: &wl_pointer::WlPointer,
        ev: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match ev {
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                st.ptr = (surface_x, surface_y);
                let hover = st.row_at(surface_x, surface_y);
                if hover != st.hover {
                    st.hover = hover;
                    st.render();
                }
            }
            wl_pointer::Event::Leave { .. } => {
                if st.hover.take().is_some() {
                    st.render();
                }
            }
            wl_pointer::Event::Button {
                button,
                state: WEnum::Value(wl_pointer::ButtonState::Pressed),
                ..
            } => {
                if button != BTN_LEFT {
                    return;
                }
                match st.row_at(st.ptr.0, st.ptr.1) {
                    Some(row) => st.choose(row),
                    // A click on the scrim closes, like every other overlay in
                    // this crate; one inside the panel but not on a row does
                    // nothing, so a click on the filter field is not a way to
                    // lose what you were looking at.
                    None => {
                        if !st.inside_panel(st.ptr.0, st.ptr.1) {
                            st.done = true;
                        }
                    }
                }
            }
            wl_pointer::Event::Axis { value, .. } => {
                st.scroll_acc += value;
                let notches = (st.scroll_acc / WHEEL_NOTCH) as i32;
                if notches != 0 {
                    st.scroll_acc -= f64::from(notches) * WHEEL_NOTCH;
                    st.move_sel(notches, Step::Clamp);
                    st.render();
                }
            }
            _ => {}
        }
    }
}

/// `--pick`: the overlay, and the `SET` of whatever was chosen.
fn pick() -> ! {
    let (items, live) = history_for_picker();
    let Ok(conn) = Connection::connect_to_env() else {
        eprintln!("lunarclip: no hay WAYLAND_DISPLAY (¿esta la sesion arriba?)");
        std::process::exit(1);
    };
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let mut st = Picker::new(items, live);
    for _ in 0..2 {
        if queue.roundtrip(&mut st).is_err() {
            eprintln!("lunarclip: fallo el roundtrip de wayland");
            std::process::exit(1);
        }
    }
    if st.layer_shell.is_none() || st.shm.is_none() || st.compositor.is_none() {
        eprintln!("lunarclip: al compositor le falta wlr-layer-shell/wl_shm");
        std::process::exit(1);
    }
    st.map_overlay(&qh);
    while !st.done {
        if queue.blocking_dispatch(&mut st).is_err() {
            break;
        }
    }
    let picked = st.picked;
    // Tear the overlay down BEFORE the selection is set, so the window that
    // gets the paste is already the focused one when it happens.
    drop(st);
    let _ = conn.flush();
    let Some(index) = picked else {
        // Esc is not a failure of the picker, but it is "nothing was chosen",
        // and a caller in a script has no other way to tell.
        std::process::exit(1)
    };
    match ask(&Req::Set(index)) {
        Some(a) if a.starts_with(b"OK") => std::process::exit(0),
        Some(a) => {
            eprintln!(
                "lunarclip: el demonio dijo: {}",
                String::from_utf8_lossy(&a).trim()
            );
            std::process::exit(1)
        }
        None => {
            eprintln!("lunarclip: no hay lunarclip --watch; no hay quien ponga la seleccion");
            std::process::exit(1)
        }
    }
}

// ── the offline render ───────────────────────────────────────────────────────

/// Floors for `--dump`, so a tiny request still renders something a person can
/// look at. The ceilings are `fill_guard::check_buffer`'s.
const MIN_DUMP_W: usize = 320;
const MIN_DUMP_H: usize = 240;

/// Split `--dump PATH:WxH` into its three parts, defaulting the size.
///
/// The size is the last `:` segment ONLY when it is digits, `x`, digits. A
/// looser rule cost `lunarrun` a bug where `--dump /tmp/frame:extra.raw` wrote
/// `/tmp/frame` at the default size, losing part of the path AND the requested
/// size without a word.
fn parse_dump_spec(spec: &str) -> (String, usize, usize) {
    let dims = spec.rsplit_once(':').and_then(|(path, dims)| {
        let (ws, hs) = dims.split_once('x')?;
        let digits = |t: &str| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit());
        if !digits(ws) || !digits(hs) {
            return None;
        }
        Some((path.to_string(), ws.parse().ok()?, hs.parse().ok()?))
    });
    dims.unwrap_or((spec.to_string(), 1280, 720))
}

/// Everything `--dump PATH:WxH` decides: the path, the size, the floors and the
/// ceilings — the same ceilings `configure` applies, in one place, so the
/// offline render cannot stand in for a surface it would not have fitted.
fn dump_target(spec: &str) -> Result<(String, usize, usize), fill_guard::TooBig> {
    let (path, w, h) = parse_dump_spec(spec);
    let (w, h) = (w.max(MIN_DUMP_W), h.max(MIN_DUMP_H));
    fill_guard::check_buffer(w, h)?;
    Ok((path, w, h))
}

/// Render the picker to a file and exit. No compositor, no seat, no daemon
/// needed: this is how the overlay is checked where there is no session.
fn dump(spec: &str) -> ! {
    let (path, w, h) = match dump_target(spec) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("lunarclip: {spec}: pasa de {e:?}; nada escrito");
            std::process::exit(1);
        }
    };
    let (items, live) = history_for_picker();
    let st = Picker::new(items, live);
    // try_new, not new: refusing beats aborting (`panic = "abort"`).
    let Some(mut cv) = Canvas::try_new(w, h) else {
        eprintln!("lunarclip: no se pudo reservar un lienzo de {w}x{h}");
        std::process::exit(1);
    };
    let (_, _, rows) = draw_picker(&mut cv, w, h, &st);
    let mut buf = vec![0u8; w * h * 4];
    if !cv.blit_argb(&mut buf) {
        eprintln!("lunarclip: fallo el blit");
        std::process::exit(1);
    }
    if let Err(e) = std::fs::write(&path, &buf) {
        eprintln!("lunarclip: {path}: {e}");
        std::process::exit(1);
    }
    println!(
        "lunarclip: escrito {path} ({w}x{h} ARGB8888, {} entradas, {rows} filas)",
        st.items.len()
    );
    std::process::exit(0)
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn cli(args: &[&str]) -> Result<Cli, String> {
        parse_args_from(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn no_arguments_is_the_picker() {
        // What Super+V runs. The default matters: a keybind that typed nothing
        // and got the daemon would start a second one on every press.
        assert_eq!(cli(&[]).unwrap().mode, Mode::Pick);
    }

    #[test]
    fn every_mode_has_its_flag() {
        for (flag, mode) in [
            ("--watch", Mode::Watch),
            ("--daemon", Mode::Watch),
            ("--pick", Mode::Pick),
            ("--copy", Mode::Copy),
            ("--paste", Mode::Paste),
            ("--list", Mode::List),
            ("--clear", Mode::Clear),
            ("--check", Mode::Check),
        ] {
            assert_eq!(cli(&[flag]).unwrap().mode, mode, "{flag}");
        }
    }

    #[test]
    fn two_different_modes_are_a_mistake_and_not_a_silent_choice() {
        assert!(cli(&["--watch", "--paste"]).is_err());
        // The same one twice is not: `--copy --copy` is a typo with one
        // meaning.
        assert_eq!(cli(&["--copy", "--copy"]).unwrap().mode, Mode::Copy);
    }

    #[test]
    fn bare_words_are_the_text_of_a_copy() {
        let c = cli(&["--copy", "hola", "mundo"]).unwrap();
        assert_eq!(c.text.as_deref(), Some("hola mundo"));
        // And belong to nothing else: a stray word beside `--paste` is a
        // mistyped command, not a selection.
        assert!(cli(&["--paste", "hola"]).is_err());
        assert!(cli(&["hola"]).is_err());
    }

    #[test]
    fn copy_with_no_text_reads_stdin() {
        assert_eq!(cli(&["--copy"]).unwrap().text, None);
    }

    #[test]
    fn the_flags_that_are_not_modes_stack_on_one() {
        let c = cli(&["--watch", "--no-persist"]).unwrap();
        assert_eq!(c.mode, Mode::Watch);
        assert!(c.no_persist);
        let c = cli(&["--copy", "--trim", "x"]).unwrap();
        assert!(c.trim);
        assert_eq!(c.text.as_deref(), Some("x"));
    }

    #[test]
    fn dump_takes_a_value_and_refuses_to_eat_the_next_flag() {
        let c = cli(&["--dump", "/tmp/f.raw:800x600"]).unwrap();
        assert_eq!(c.dump.as_deref(), Some("/tmp/f.raw:800x600"));
        // `--dump --trim` is a forgotten path. Writing a frame to a file
        // called `--trim` helps nobody.
        assert!(cli(&["--dump", "--trim"]).is_err());
        assert!(cli(&["--dump"]).is_err());
    }

    #[test]
    fn an_unknown_option_is_refused_rather_than_taken_as_text() {
        assert!(cli(&["--wach"]).is_err());
        assert!(cli(&["-x"]).is_err());
        assert_eq!(cli(&["--help"]).unwrap_err(), "help");
    }

    #[test]
    fn a_dump_spec_keeps_its_path_when_the_size_is_not_a_size() {
        assert_eq!(
            parse_dump_spec("/tmp/f.raw:800x600"),
            ("/tmp/f.raw".to_string(), 800, 600)
        );
        // The whole spec is the path, and the size is the default: no segment
        // of the path is lost.
        assert_eq!(
            parse_dump_spec("/tmp/frame:extra.raw"),
            ("/tmp/frame:extra.raw".to_string(), 1280, 720)
        );
        assert_eq!(
            parse_dump_spec("/tmp/f.raw"),
            ("/tmp/f.raw".to_string(), 1280, 720)
        );
        // `+8` parses as 8 and must not be taken as a size.
        assert_eq!(
            parse_dump_spec("/tmp/f.raw:+8x600"),
            ("/tmp/f.raw:+8x600".to_string(), 1280, 720)
        );
    }

    #[test]
    fn a_dump_is_floored_and_a_huge_one_is_refused() {
        let (_, w, h) = dump_target("/tmp/f.raw:1x1").unwrap();
        assert_eq!((w, h), (MIN_DUMP_W, MIN_DUMP_H));
        assert!(dump_target("/tmp/f.raw:100000x100000").is_err());
    }

    #[test]
    fn a_zero_configure_is_a_size_this_client_chooses() {
        assert_eq!(surface_size(0, 0), Some((1280, 720)));
        assert_eq!(surface_size(640, 480), Some((640, 480)));
        assert_eq!(surface_size(99999, 99999), None);
    }

    #[test]
    fn the_panel_fits_inside_a_small_output() {
        for (ow, oh) in [
            (320, 240),
            (640, 480),
            (800, 600),
            (1280, 720),
            (3840, 2160),
        ] {
            let (x, y, w, h) = panel_rect(ow, oh, 50);
            assert!(x >= 0 && y >= 0, "{ow}x{oh}: {x},{y}");
            assert!(x + w <= ow, "{ow}x{oh}: {x}+{w}");
            assert!(y + h <= oh, "{ow}x{oh}: {y}+{h}");
            // And it is never narrower than the floor, unless the output
            // itself is: a 224-pixel panel fits on the screen and shows no
            // entry anybody can read.
            assert!(w >= MIN_PANEL_W.min(ow), "{ow}x{oh}: {w} de ancho");
            assert!(rows_fitting(h) >= 1);
        }
    }

    #[test]
    fn a_tall_panel_shows_at_most_the_row_cap() {
        let (_, _, _, h) = panel_rect(1920, 2000, 500);
        assert_eq!(rows_fitting(h), MAX_ROWS);
    }

    #[test]
    fn the_filter_keeps_the_index_each_row_has_in_the_whole_history() {
        // The index is what `SET` takes, so a filtered list that renumbered its
        // rows would paste the wrong entry — the one bug in this program that
        // the user would discover by pasting a password into a chat window.
        let mut p = Picker::new(
            vec![
                "cero".to_string(),
                "uno".to_string(),
                "dos".to_string(),
                "tres".to_string(),
            ],
            true,
        );
        p.field = "s".to_string();
        p.refilter();
        assert_eq!(
            p.rows.iter().map(|(i, _)| *i).collect::<Vec<_>>(),
            vec![2, 3]
        );
        p.choose(1);
        assert_eq!(p.picked, Some(3));
        assert!(p.done);
    }

    #[test]
    fn choosing_a_row_that_is_not_there_picks_nothing_and_still_closes() {
        let mut p = Picker::new(Vec::new(), true);
        p.choose(0);
        assert_eq!(p.picked, None);
        assert!(p.done);
    }

    #[test]
    fn filtering_puts_the_selection_back_on_the_first_row() {
        let mut p = Picker::new((0..30).map(|i| format!("e{i}")).collect(), true);
        p.sel = 20;
        p.top = 12;
        p.field = "e1".to_string();
        p.refilter();
        assert_eq!((p.sel, p.top), (0, 0));
    }

    #[test]
    fn the_rows_are_one_line_each_however_the_entry_looked() {
        let p = Picker::new(vec!["una\nentrada\nde\ntres".to_string()], true);
        assert!(!p.rows[0].1.contains('\n'));
    }

    #[test]
    fn the_offline_render_draws_the_picker_at_every_size() {
        // The whole point of `--dump`: this is the only way the overlay is
        // checked where there is no compositor, and a panic here is a panic on
        // the user's screen (`panic = "abort"`).
        let p = Picker::new(
            (0..40)
                .map(|i| format!("entrada numero {i}\ncon salto"))
                .collect(),
            true,
        );
        for (w, h) in [(320, 240), (640, 480), (1280, 720), (1920, 1080)] {
            let mut cv = Canvas::try_new(w, h).unwrap();
            let ((px, py, pw, ph), row0, rows) = draw_picker(&mut cv, w, h, &p);
            assert!(px >= 0 && py >= 0 && pw > 0 && ph > 0, "{w}x{h}");
            assert!(row0 >= py, "{w}x{h}");
            assert!((1..=MAX_ROWS).contains(&rows), "{w}x{h}: {rows}");
        }
    }

    /// A pollfd as the daemon builds them.
    fn pfd(fd: i32, revents: i16) -> libc::pollfd {
        libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents,
        }
    }

    #[test]
    fn the_poll_answer_names_client_descriptors_and_never_the_fixed_two() {
        // The first two entries are the compositor and the listening socket.
        // Returning either of them as a "client" would have the daemon read a
        // request off its own Wayland connection.
        let pfds = [
            pfd(3, libc::POLLIN),
            pfd(4, libc::POLLIN),
            pfd(42, libc::POLLIN),
            pfd(43, 0),
            pfd(44, libc::POLLHUP),
        ];
        assert_eq!(ready_fds(&pfds, POLL_FIXED), vec![42, 44]);
        // Nothing ready is nothing to serve, not "all of them".
        let quiet = [pfd(3, 0), pfd(4, 0), pfd(42, 0)];
        assert!(ready_fds(&quiet, POLL_FIXED).is_empty());
    }

    #[test]
    fn a_client_accepted_this_round_is_not_looked_up_in_the_poll_set() {
        // THE CRASH lunarclip shipped with: the poll ran with no clients, so
        // `pfds` had two entries; the accept pushed the first connection ever
        // made to the daemon; and the walk indexed `pfds[0 + 2]` -- "len is 2
        // but the index is 2". With `panic = "abort"` that killed the daemon,
        // which means every `lunarclip --check`, `--list`, `--pick` or `--copy`
        // emptied the clipboard and left eclipse-init respawning into the same
        // abort.
        let pfds = [pfd(3, 0), pfd(4, libc::POLLIN)];
        let ready = ready_fds(&pfds, POLL_FIXED);
        // The listener was the one that was ready, and it is not a client.
        assert!(ready.is_empty());
        // The walk over the grown client list finds nothing to do and, above
        // all, indexes nothing: the new client is read on the next poll.
        let accepted = [77];
        let served: Vec<i32> = accepted
            .iter()
            .copied()
            .filter(|fd| ready.contains(fd))
            .collect();
        assert!(served.is_empty());
    }

    #[test]
    fn an_old_client_is_served_while_a_new_one_waits_its_turn() {
        // One client was in the poll set and spoke; another connected in the
        // same round. Exactly the first is served, and by ITS descriptor, so a
        // removal earlier in the walk cannot hand the next iteration somebody
        // else's `revents`.
        let pfds = [pfd(3, 0), pfd(4, libc::POLLIN), pfd(42, libc::POLLIN)];
        let ready = ready_fds(&pfds, POLL_FIXED);
        let clients = [42, 77];
        let served: Vec<i32> = clients
            .iter()
            .copied()
            .filter(|fd| ready.contains(fd))
            .collect();
        assert_eq!(served, vec![42]);
    }

    #[test]
    fn a_stale_socket_is_replaced_and_a_live_one_is_refused() {
        // This is what a respawn looks like: eclipse-init restarts the daemon
        // and the socket file of the one that was killed is still there. A
        // bind that failed on it would leave the clipboard dead for the rest
        // of the session, with `EADDRINUSE` in a log nobody reads.
        let dir = std::env::temp_dir().join(format!("lunarclip-sock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("lunarclip.sock");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, b"no soy un socket").unwrap();
        let listener = bind_socket(&path).expect("a stale socket must not stop the daemon");
        // And while THAT one is listening, a second daemon is refused rather
        // than taking the socket from under it.
        let again = bind_socket(&path);
        assert!(again.is_err(), "a second daemon must not bind");
        assert_eq!(
            again.unwrap_err().kind(),
            std::io::ErrorKind::AddrInUse,
            "and must say why"
        );
        drop(listener);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_history_draws_the_empty_message_and_not_a_panic() {
        let p = Picker::new(Vec::new(), false);
        let mut cv = Canvas::try_new(800, 600).unwrap();
        let (_, _, rows) = draw_picker(&mut cv, 800, 600, &p);
        assert_eq!(rows, rows.max(1));
    }
}
