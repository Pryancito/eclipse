//! `eclipse-notifyd` — Eclipse OS's notification server.
//!
//! It owns `org.freedesktop.Notifications` on the session bus and turns each
//! `Notify` into a card on the desktop, by dropping an alert file into the
//! spool the bar already shows hunter's alerts from (`/run/hunter/notify`).
//! When the bar answers (a button, the close cross, or the card running out of
//! time), the answer goes back to the program as `ActionInvoked` and
//! `NotificationClosed`.
//!
//! The logic lives in `eclipse_dbusd::notify`, which is pure and tested. What
//! is here is the socket, the files and the bookkeeping.
//!
//! ```text
//! eclipse-notifyd [--address=unix:path=/run/user/0/bus] [--spool=DIR]
//! ```

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use eclipse_dbusd::client::Client;
use eclipse_dbusd::message::{Arg, Message, FLAG_NO_REPLY_EXPECTED, MSG_METHOD_CALL};
use eclipse_dbusd::notify::{self, Call, Note};

/// How long one turn of the loop waits on the socket before looking at the
/// spool for answers. A button press reaches the program within this.
const SLICE: Duration = Duration::from_millis(250);

/// Notifications remembered at once. A program that notifies and never looks
/// again leaves an entry until the card goes; past this the oldest is
/// forgotten (its card stays, its answer is simply not forwarded).
const MAX_LIVE: usize = 256;

fn log(msg: &str) {
    println!("[eclipse-notifyd] {msg}");
    let _ = std::io::stdout().flush();
}

/// One notification on screen.
struct Live {
    note: Note,
    /// Its alert's id in the spool.
    spool_id: String,
    /// Who to tell how it went.
    sender: String,
}

fn main() {
    let mut address: Option<String> = None;
    let mut spool = PathBuf::from(notify::SPOOL);
    for a in std::env::args().skip(1) {
        if let Some(v) = a.strip_prefix("--address=") {
            address = Some(v.to_string());
        } else if let Some(v) = a.strip_prefix("--spool=") {
            spool = PathBuf::from(v);
        } else {
            match a.as_str() {
                "--version" => {
                    println!("eclipse-notifyd {}", env!("CARGO_PKG_VERSION"));
                    return;
                }
                "--help" | "-h" => {
                    println!(
                        "usage: eclipse-notifyd [--address=unix:path=...] [--spool=DIR]\n\
                         \n\
                         \x20 serves org.freedesktop.Notifications on the session bus,\n\
                         \x20 showing each notification as a card of the bar."
                    );
                    return;
                }
                other => {
                    eprintln!("eclipse-notifyd: unknown option {other}");
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
    let Some(path) = eclipse_dbusd::socket_path(&address) else {
        eprintln!("eclipse-notifyd: no unix:path= in {address}");
        std::process::exit(2);
    };
    let mut bus = match Client::connect(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("eclipse-notifyd: {e}");
            std::process::exit(1);
        }
    };
    match bus.request_name(notify::NAME) {
        Ok(true) => log(&format!("{} on unix:path={path}", notify::NAME)),
        Ok(false) => {
            // Another notification daemon (mako, dunst) is the user's choice
            // when it is running: stand aside rather than fight over the name.
            log(&format!("{} ya tiene dueno; no hago nada", notify::NAME));
            // Parked, not exited: init respawns a service that exits, and a
            // loop of "name taken" lines helps nobody.
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        }
        Err(e) => {
            eprintln!("eclipse-notifyd: RequestName: {e}");
            std::process::exit(1);
        }
    }

    let mut live: BTreeMap<u32, Live> = BTreeMap::new();
    let mut next_id: u32 = 1;
    loop {
        match bus.poll(SLICE) {
            Ok(Some(msg)) if msg.kind == MSG_METHOD_CALL => {
                serve(&mut bus, &msg, &mut live, &mut next_id, &spool);
            }
            Ok(_) => {}
            Err(e) => {
                log(&format!("el bus se fue: {e}"));
                return;
            }
        }
        reap(&mut bus, &mut live, &spool);
    }
}

fn serve(
    bus: &mut Client,
    msg: &Message,
    live: &mut BTreeMap<u32, Live>,
    next_id: &mut u32,
    spool: &Path,
) {
    let path = msg.path.clone().unwrap_or_default();
    match notify::classify(&path, msg.interface.as_deref(), msg.member.as_deref()) {
        Call::Notify => {
            let Some(note) = notify::note_from(&msg.args()) else {
                return reply_error(bus, msg, "InvalidArgs", "Notify(susssasa{sv}i)");
            };
            // A replacement takes the old card down quietly and keeps its id:
            // that is how a progress notification updates in place.
            let id = match live.remove(&note.replaces) {
                Some(old) => {
                    withdraw(spool, &old.spool_id);
                    note.replaces
                }
                None => {
                    let id = *next_id;
                    *next_id = next_id.checked_add(1).unwrap_or(1).max(1);
                    id
                }
            };
            let mut r = Message::method_return(msg);
            r.set_body(&[Arg::U32(id)]);
            let _ = bus.send(r);
            match post(spool, &note) {
                Ok(spool_id) => {
                    log(&format!("{id}: {} — {}", note.app, note.summary));
                    let sender = msg.sender.clone().unwrap_or_default();
                    live.insert(
                        id,
                        Live {
                            note,
                            spool_id,
                            sender,
                        },
                    );
                    while live.len() > MAX_LIVE {
                        live.pop_first();
                    }
                }
                Err(e) => log(&format!("no pude escribir en {}: {e}", spool.display())),
            }
        }
        Call::Close => {
            let id = match msg.args().first() {
                Some(Arg::U32(v)) => *v,
                _ => 0,
            };
            if let Some(l) = live.remove(&id) {
                withdraw(spool, &l.spool_id);
                closed(bus, &l.sender, id, notify::CLOSED_BY_CALL);
            }
            let _ = bus.send(Message::method_return(msg));
        }
        Call::Capabilities => {
            let mut r = Message::method_return(msg);
            r.set_body(&[notify::capabilities()]);
            let _ = bus.send(r);
        }
        Call::ServerInformation => {
            let mut r = Message::method_return(msg);
            r.set_body(&notify::server_information());
            let _ = bus.send(r);
        }
        Call::Introspect => {
            let mut r = Message::method_return(msg);
            r.set_body(&[Arg::Str(notify::introspection())]);
            let _ = bus.send(r);
        }
        Call::Ping => {
            let _ = bus.send(Message::method_return(msg));
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

/// Forward every answer the bar has written since the last turn.
fn reap(bus: &mut Client, live: &mut BTreeMap<u32, Live>, spool: &Path) {
    let mut done = Vec::new();
    for (id, l) in live.iter() {
        let answer = spool.join(format!("{}.answer", l.spool_id));
        let Ok(text) = std::fs::read_to_string(&answer) else {
            continue;
        };
        let _ = std::fs::remove_file(&answer);
        done.push((*id, text.trim().to_string()));
    }
    for (id, answer) in done {
        let Some(l) = live.remove(&id) else { continue };
        if let Some(key) = notify::key_of(&l.note, &answer) {
            let mut sig = Message::signal(notify::PATH, notify::IFACE, "ActionInvoked");
            sig.set_body(&[Arg::U32(id), Arg::Str(key.to_string())]);
            let _ = bus.send(sig);
        }
        closed(bus, &l.sender, id, notify::closed_reason(&answer));
    }
}

/// `NotificationClosed`. Broadcast, as the specification has it: libnotify
/// subscribes with a match rule before it calls `Notify`, and a broadcast is
/// also what lets anything else watching the bus (a history, a test) see it.
fn closed(bus: &mut Client, _sender: &str, id: u32, reason: u32) {
    let mut sig = Message::signal(notify::PATH, notify::IFACE, "NotificationClosed");
    sig.set_body(&[Arg::U32(id), Arg::U32(reason)]);
    let _ = bus.send(sig);
}

/// Take a card down: its alert file goes, and the bar drops a card whose file
/// is gone on its next pass.
fn withdraw(spool: &Path, spool_id: &str) {
    let _ = std::fs::remove_file(spool.join(format!("{spool_id}.alert")));
    let _ = std::fs::remove_file(spool.join(format!("{spool_id}.answer")));
}

/// Drop `note` into the spool and return its id. The file is written under a
/// dot-name and renamed, so the bar never reads half of it; the directory and
/// the file are private, because the bar trusts only what nobody else can
/// write.
fn post(spool: &Path, note: &Note) -> std::io::Result<String> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(spool)?;
    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let id = format!("{secs}-{}-{seq}", std::process::id());
    let tmp = spool.join(format!(".{id}"));
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(notify::render(note).as_bytes())?;
    }
    std::fs::rename(&tmp, spool.join(format!("{id}.alert")))?;
    Ok(id)
}

fn reply_error(bus: &mut Client, msg: &Message, kind: &str, text: &str) {
    if msg.flags & FLAG_NO_REPLY_EXPECTED != 0 {
        return;
    }
    let name = format!("org.freedesktop.DBus.Error.{kind}");
    let _ = bus.send(Message::error(msg, &name, text));
}
