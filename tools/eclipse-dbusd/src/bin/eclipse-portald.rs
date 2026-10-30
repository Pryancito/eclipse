//! `eclipse-portald` — Eclipse OS's XDG desktop portal.
//!
//! What it is for
//! --------------
//! The "Open file" of a modern toolkit is a D-Bus call, not a widget. GTK with
//! `GTK_USE_PORTAL=1` (and unconditionally inside a sandbox), Qt's
//! `QFileDialog` with the xdg-desktop-portal platform theme, Firefox's file
//! picker and `xdg-open` all ask
//! `org.freedesktop.portal.FileChooser.OpenFile` and then wait for a signal.
//! Eclipse had both ends and nothing in between: a session bus
//! (`eclipse-dbusd`) and a file chooser (`lunarfile --pick`). This is the piece
//! that joins them, so that an application which refuses to draw its own dialog
//! still gets an answer.
//!
//! How it works
//! ------------
//! One connection, one loop. A `FileChooser` call is answered **at once** with
//! a request handle, the dialog is started as a child process writing its
//! answer to a file only this daemon can name, and when the child exits the
//! answer goes back as a `Response` signal on that handle. The loop polls the
//! socket in short slices so that a dialog finishing is noticed while the bus
//! is quiet, and the daemon stays able to serve a second call while the first
//! dialog is still up.
//!
//! The logic lives in `eclipse_dbusd::portal`, which is pure and tested. What
//! is here is the socket, the child and the bookkeeping.
//!
//! Running it
//! ----------
//! ```text
//! eclipse-portald [--address=unix:path=/run/user/0/bus] [--picker=lunarfile]
//!                 [--answers=DIR] [--ready=FILE]
//! ```
//! `eclipse-init` supervises it like every other service
//! (`/etc/eclipse/services/portal.service`). `--ready` is how the session
//! decides whether to export `GTK_USE_PORTAL=1`: the file appears only once the
//! portal owns its name, so a session whose portal failed to start keeps the
//! toolkit's own dialog instead of being pointed at a portal that is not there.

use std::collections::HashMap;
use std::io::Write;
use std::process::{Child, Command};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use eclipse_dbusd::client::Client;
use eclipse_dbusd::message::{Arg, Message, FLAG_NO_REPLY_EXPECTED, MSG_METHOD_CALL};
use eclipse_dbusd::portal::{
    self, Ask, Call, FILE_CHOOSER_VERSION, IFACE_FILE_CHOOSER, PORTAL_NAME, RESPONSE_CANCELLED,
    RESPONSE_OK, RESPONSE_OTHER,
};

/// How long one turn of the loop waits on the socket before looking at the
/// dialogs. Short enough that a chosen file reaches the application without a
/// visible pause, long enough that an idle session is not a spin: a quiet
/// portal wakes five times a second and does nothing.
const SLICE: Duration = Duration::from_millis(200);

/// Where the answers go when `--answers` says nothing. It is under
/// `/run/user/0`, which is the session's own tmpfs, and it is created 0700.
const DEFAULT_ANSWER_DIR: &str = "/run/user/0/eclipse-portal";

/// The chooser, when `--picker` says nothing. Resolved through `PATH` by the
/// shell, which is what finds it in `/bin` on the image and in a build tree
/// when a developer puts it earlier in `PATH`.
const DEFAULT_PICKER: &str = "lunarfile";

/// The session's runtime directory when `XDG_RUNTIME_DIR` is unset, which is
/// where the compositor's socket is looked for. It is the one `eclipse-init`
/// gives every service, and the one the service files wait on.
const DEFAULT_RUNTIME_DIR: &str = "/run/user/0";

/// How long a dialog may stay up before the portal stops waiting for it.
///
/// Without a bound a client that is never answered is a `Response` that never
/// comes, and the application is wedged with no dialog on screen to close --
/// which is the failure mode of a portal whose chooser died without writing its
/// file. Half an hour is far longer than any real "choose a file", and the
/// answer when it fires is `CANCELLED`, so the application carries on.
const DIALOG_TIMEOUT: Duration = Duration::from_secs(1800);

fn log(msg: &str) {
    println!("[eclipse-portald] {msg}");
    let _ = std::io::stdout().flush();
}

/// One dialog in flight.
struct Pending {
    /// The request object the client is listening on.
    handle: String,
    /// Who to send the `Response` to.
    sender: String,
    /// The file the chooser writes.
    answer: String,
    /// `SaveFile`: the answer comes back marked writable.
    save: bool,
    child: Child,
    started: Instant,
}

fn main() {
    let mut address: Option<String> = None;
    let mut picker = DEFAULT_PICKER.to_string();
    let mut answers = DEFAULT_ANSWER_DIR.to_string();
    let mut ready: Option<String> = None;

    for a in std::env::args().skip(1) {
        if let Some(v) = a.strip_prefix("--address=") {
            address = Some(v.to_string());
        } else if let Some(v) = a.strip_prefix("--picker=") {
            picker = v.to_string();
        } else if let Some(v) = a.strip_prefix("--answers=") {
            answers = v.to_string();
        } else if let Some(v) = a.strip_prefix("--ready=") {
            ready = Some(v.to_string());
        } else {
            match a.as_str() {
                "--version" => {
                    println!("eclipse-portald {}", env!("CARGO_PKG_VERSION"));
                    return;
                }
                "--help" | "-h" => {
                    println!(
                        "usage: eclipse-portald [--address=unix:path=...] [--picker=PROG]\n\
                         \x20                      [--answers=DIR] [--ready=FILE]\n\
                         \n\
                         \x20 serves org.freedesktop.portal.FileChooser on the session bus,\n\
                         \x20 showing the dialog with `lunarfile --pick`."
                    );
                    return;
                }
                other => {
                    eprintln!("eclipse-portald: unknown option {other}");
                    std::process::exit(2);
                }
            }
        }
    }

    let address = eclipse_dbusd::chosen_address(
        address,
        false,
        std::env::var("DBUS_SESSION_BUS_ADDRESS").ok(),
    );
    let path = match eclipse_dbusd::socket_path(&address) {
        Some(p) => p,
        None => {
            eprintln!("eclipse-portald: no unix:path= in {address}");
            std::process::exit(2);
        }
    };

    if let Err(e) = std::fs::create_dir_all(&answers) {
        eprintln!("eclipse-portald: {answers}: {e}");
        std::process::exit(1);
    }
    // Nobody but this daemon has any business reading an answer: it names the
    // file an application is about to open.
    set_private(&answers);

    let mut bus = match Client::connect(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("eclipse-portald: {e}");
            std::process::exit(1);
        }
    };
    match bus.request_name(PORTAL_NAME) {
        Ok(true) => log(&format!("{PORTAL_NAME} on unix:path={path}")),
        Ok(false) => {
            // Alpine's own xdg-desktop-portal may be installed; it is the
            // better portal and it is already there, so stand aside quietly
            // rather than fight over the name.
            log(&format!("{PORTAL_NAME} ya tiene dueno; no hago nada"));
            return;
        }
        Err(e) => {
            eprintln!("eclipse-portald: RequestName: {e}");
            std::process::exit(1);
        }
    }
    if let Some(stamp) = &ready {
        // Written only now, with the name in hand: the session reads it to
        // decide whether to export GTK_USE_PORTAL=1, and a portal that is not
        // serving must not be advertised.
        if let Err(e) = std::fs::write(stamp, format!("{PORTAL_NAME}\n")) {
            log(&format!("no pude escribir {stamp}: {e}"));
        }
    }

    let machine_id = std::fs::read_to_string("/etc/machine-id")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let mut pending: HashMap<String, Pending> = HashMap::new();
    let mut seq: u64 = 0;

    loop {
        match bus.poll(SLICE) {
            Ok(Some(msg)) => {
                if msg.kind == MSG_METHOD_CALL {
                    serve(
                        &mut bus,
                        &msg,
                        &mut pending,
                        &picker,
                        &answers,
                        &mut seq,
                        &machine_id,
                    );
                }
            }
            Ok(None) => {}
            Err(e) => {
                // The bus going away ends the session; `eclipse-init` restarts
                // the daemon when the bus comes back.
                log(&format!("el bus se fue: {e}"));
                return;
            }
        }
        reap(&mut bus, &mut pending);
    }
}

/// Answer one method call.
#[allow(clippy::too_many_arguments)]
fn serve(
    bus: &mut Client,
    msg: &Message,
    pending: &mut HashMap<String, Pending>,
    picker: &str,
    answers: &str,
    seq: &mut u64,
    machine_id: &str,
) {
    let path = msg.path.clone().unwrap_or_default();
    let what = portal::classify(&path, msg.interface.as_deref(), msg.member.as_deref());
    match what {
        Call::Choose(member) => {
            let args = msg.args();
            let title = match args.get(1) {
                Some(Arg::Str(s)) => s.clone(),
                _ => String::new(),
            };
            let opts: Vec<(String, Arg)> = match args.get(2) {
                Some(Arg::DictSV(d)) => d.clone(),
                _ => Vec::new(),
            };
            let ask = match portal::ask_from(&member, &title, &opts) {
                Some(a) => a,
                None => return reply_error(bus, msg, "UnknownMethod", "ese metodo no esta"),
            };
            let sender = msg.sender.clone().unwrap_or_default();
            let token = portal::opt_text(&opts, "handle_token").unwrap_or_default();
            let handle = portal::request_path(&sender, &token);
            // The handle goes back BEFORE the dialog is up: the client is
            // waiting for the reply before it starts listening for the signal.
            let mut r = Message::method_return(msg);
            r.set_body(&[Arg::Path(handle.clone())]);
            let _ = bus.send(r);

            if let Some(why) = ask.unsupported() {
                log(&format!("{member}: {why}"));
                respond(bus, &handle, &sender, RESPONSE_OTHER, &[], false);
                return;
            }
            *seq += 1;
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let answer = portal::answer_file(answers, std::process::id(), nanos, *seq);
            match launch(picker, &ask, &answer) {
                Ok(child) => {
                    log(&format!("{member} de {sender} -> {handle}"));
                    pending.insert(
                        handle.clone(),
                        Pending {
                            handle,
                            sender,
                            answer,
                            save: ask.save,
                            child,
                            started: Instant::now(),
                        },
                    );
                }
                Err(e) => {
                    log(&format!("no pude abrir el dialogo: {e}"));
                    respond(bus, &handle, &sender, RESPONSE_OTHER, &[], false);
                }
            }
        }
        Call::CloseRequest => {
            // The client is no longer interested: take the dialog down and emit
            // nothing. A `Response` after a `Close` is an answer to a question
            // the application has stopped listening for, and libportal treats
            // the handle as gone.
            if let Some(mut p) = pending.remove(&path) {
                let _ = p.child.kill();
                let _ = p.child.wait();
                let _ = std::fs::remove_file(&p.answer);
                log(&format!("Close de {path}"));
            }
            let _ = bus.send(Message::method_return(msg));
        }
        Call::Version => {
            // `Get(interface, property)`. Only `version` exists, and only on
            // the FileChooser interface.
            let args = msg.args();
            let iface = match args.first() {
                Some(Arg::Str(s)) => s.clone(),
                _ => String::new(),
            };
            let prop = match args.get(1) {
                Some(Arg::Str(s)) => s.clone(),
                _ => String::new(),
            };
            if iface == IFACE_FILE_CHOOSER && prop == "version" {
                let mut r = Message::method_return(msg);
                r.set_body(&[Arg::Variant(Box::new(Arg::U32(FILE_CHOOSER_VERSION)))]);
                let _ = bus.send(r);
            } else {
                reply_error(
                    bus,
                    msg,
                    "UnknownProperty",
                    &format!("{iface}.{prop} no existe"),
                );
            }
        }
        Call::AllProperties => {
            let mut r = Message::method_return(msg);
            r.set_body(&[Arg::DictSV(vec![(
                "version".to_string(),
                Arg::U32(FILE_CHOOSER_VERSION),
            )])]);
            let _ = bus.send(r);
        }
        Call::Introspect => {
            let mut r = Message::method_return(msg);
            r.set_body(&[Arg::Str(portal::introspection(&path))]);
            let _ = bus.send(r);
        }
        Call::Ping => {
            let _ = bus.send(Message::method_return(msg));
        }
        Call::MachineId => {
            let mut r = Message::method_return(msg);
            r.set_body(&[Arg::Str(machine_id.to_string())]);
            let _ = bus.send(r);
        }
        Call::NoSuchObject => reply_error(
            bus,
            msg,
            "UnknownObject",
            &format!("no sirvo el objeto {path}"),
        ),
        Call::NoSuchMethod => reply_error(bus, msg, "UnknownMethod", "ese metodo no esta"),
    }
}

/// Send an `org.freedesktop.DBus.Error.<kind>`, unless the caller said it does
/// not want a reply (in which case an error is forbidden, not merely useless).
fn reply_error(bus: &mut Client, msg: &Message, kind: &str, text: &str) {
    if msg.flags & FLAG_NO_REPLY_EXPECTED != 0 {
        return;
    }
    let name = format!("org.freedesktop.DBus.Error.{kind}");
    let _ = bus.send(Message::error(msg, &name, text));
}

/// Emit `Response` on a request handle, addressed to the client that asked.
///
/// It is sent **unicast**: the bus delivers it to the sender by destination and
/// also to anyone whose match rule covers it, so a client that subscribed in
/// the usual way still sees it and one that did not subscribe at all is not
/// left waiting. A broadcast alone would depend on the client having added its
/// match rule before the user clicked, which is exactly the race the portal
/// protocol was shaped to avoid.
fn respond(bus: &mut Client, handle: &str, sender: &str, code: u32, uris: &[String], save: bool) {
    let mut sig = Message::signal(handle, portal::IFACE_REQUEST, "Response");
    if !sender.is_empty() {
        sig.destination = Some(sender.to_string());
    }
    sig.set_body(&portal::response_body(code, uris, save));
    let _ = bus.send(sig);
}

/// Start the chooser, detached from the terminal but NOT from this process: the
/// portal needs to know when it exits, which is the whole signal that the user
/// has answered.
///
/// The child is given the SESSION, which this daemon does not have: a service
/// started by `eclipse-init` gets `XDG_RUNTIME_DIR` and deliberately no
/// `WAYLAND_DISPLAY`, since the compositor is what creates the socket and may
/// not have started yet. A chooser launched with that environment is a Wayland
/// client with no display: it exits before it maps, writes no answer, and the
/// empty answer reaches the application as a cancellation -- an "Open file"
/// that does nothing at all. So the socket is resolved HERE, at the moment the
/// dialog is asked for, and exported for the child.
fn launch(picker: &str, ask: &Ask, answer: &str) -> std::io::Result<Child> {
    let cmd = portal::picker_command(picker, ask, answer);
    let mut c = Command::new("/bin/sh");
    c.arg("-c").arg(&cmd);
    let runtime =
        std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| DEFAULT_RUNTIME_DIR.to_string());
    let entries: Vec<String> = std::fs::read_dir(&runtime)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();
    match portal::wayland_socket(&entries, std::env::var("WAYLAND_DISPLAY").ok().as_deref()) {
        Some(display) => {
            c.env("XDG_RUNTIME_DIR", &runtime);
            c.env("WAYLAND_DISPLAY", &display);
        }
        // Not a refusal: a chooser that finds the compositor by itself still
        // works (`lunarbar::wl`). But it is the first thing to look at when a
        // dialog never appears, so it is said out loud.
        None => log(&format!(
            "sin socket de Wayland en {runtime}; el dialogo tendra que buscarlo solo"
        )),
    }
    c.spawn()
}

/// Look at every dialog in flight and answer the ones that have finished.
fn reap(bus: &mut Client, pending: &mut HashMap<String, Pending>) {
    let mut done: Vec<String> = Vec::new();
    for (key, p) in pending.iter_mut() {
        let finished = match p.child.try_wait() {
            Ok(Some(_)) => true,
            // Still up. A dialog nobody ever answers is given up on, so that
            // the application is not wedged waiting for a signal.
            Ok(None) => {
                if p.started.elapsed() > DIALOG_TIMEOUT {
                    let _ = p.child.kill();
                    let _ = p.child.wait();
                    true
                } else {
                    false
                }
            }
            // `try_wait` failing means there is no child to wait for.
            Err(_) => true,
        };
        if finished {
            done.push(key.clone());
        }
    }
    for key in done {
        if let Some(p) = pending.remove(&key) {
            // The exit status is NOT the answer: `lunarfile` exits 1 when the
            // user cancelled, but a chooser killed by a signal also exits
            // non-zero with a file already written. What decides is whether
            // there are paths in the file.
            let text = std::fs::read_to_string(&p.answer).unwrap_or_default();
            let _ = std::fs::remove_file(&p.answer);
            let picked = portal::parse_picked(&text);
            let code = if picked.is_empty() {
                RESPONSE_CANCELLED
            } else {
                RESPONSE_OK
            };
            log(&format!(
                "{} -> {} ({} rutas)",
                p.handle,
                code,
                picked.len()
            ));
            respond(bus, &p.handle, &p.sender, code, &picked, p.save);
        }
    }
}

/// `chmod 0700`, best effort: the answers directory is on a tmpfs the session
/// owns, and on a single-user image the mode is defence in depth rather than
/// the thing keeping anyone out.
fn set_private(dir: &str) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(md) = std::fs::metadata(dir) {
        let mut perm = md.permissions();
        perm.set_mode(0o700);
        let _ = std::fs::set_permissions(dir, perm);
    }
}
