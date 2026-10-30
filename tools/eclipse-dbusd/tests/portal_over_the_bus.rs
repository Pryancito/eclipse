//! The portal over a real bus, from the outside.
//!
//! The unit tests in `portal.rs` cover the decisions; this covers the wiring,
//! which is where a portal actually fails: the handle has to come back BEFORE
//! the dialog is up, the `Response` has to reach the client that asked, and the
//! daemon has to keep serving while a dialog is open. None of that can be seen
//! from inside a pure function, and none of it needs a compositor: the chooser
//! is a stand-in script that writes the answer file `eclipse-portald` named.
//!
//! What it cannot cover is GTK itself. No toolkit is installed here, so the
//! proof that Firefox's dialog arrives as these messages is the specification
//! and `gdbus`, not a test.

use std::io::Write;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use eclipse_dbusd::client::Client;
use eclipse_dbusd::message::{Arg, MSG_SIGNAL};
use eclipse_dbusd::portal::{
    IFACE_FILE_CHOOSER, IFACE_REQUEST, PORTAL_NAME, PORTAL_PATH, RESPONSE_CANCELLED, RESPONSE_OK,
};

/// A bus, a portal and a stand-in chooser, torn down together.
struct World {
    sock: String,
    dir: String,
    portald: Child,
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = self.portald.kill();
        let _ = self.portald.wait();
        let _ = std::fs::remove_file(&self.sock);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn unique(tag: &str) -> String {
    format!(
        "{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

/// Wait until `cond` holds, or fail saying what was being waited for. Every
/// wait in this file is bounded: a portal that never answers must fail the
/// test, not hang the suite.
fn until<F: FnMut() -> bool>(que: &str, mut cond: F) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !cond() {
        assert!(Instant::now() < deadline, "no llego: {que}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Start a bus and a portal whose chooser is `guion` (shell, with `$OUT` as the
/// answer file it was told to write and `$@` its arguments).
fn start(guion: &str) -> World {
    start_with_env(guion, &[])
}

/// The same, with `env` added to the portal's own environment. The chooser
/// writes its environment to `$dir/env` either way, which is how a test sees
/// what the dialog was actually started with.
fn start_with_env(guion: &str, env: &[(String, String)]) -> World {
    let dir = format!("/tmp/{}", unique("eclipse-portal-e2e"));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let sock = format!("{dir}/bus");
    let s = sock.clone();
    std::thread::spawn(move || {
        let _ = eclipse_dbusd::serve(&s, false, false);
    });
    // The socket FILE appears at `bind`, before `listen`, so a connect landing
    // in between is refused: probe with a real connection.
    until("el bus escucha", || {
        std::os::unix::net::UnixStream::connect(&sock).is_ok()
    });

    // The stand-in chooser. It records its own argv and its environment, and
    // then does whatever the test asked, so one script covers every half: what
    // the portal passed in, what session it was started in, and what comes
    // back out.
    let picker = format!("{dir}/chooser");
    let mut f = std::fs::File::create(&picker).expect("crear el guion");
    writeln!(
        f,
        "#!/bin/sh\nOUT=\nprev=\nfor a in \"$@\"; do\n  if [ \"$prev\" = --out ]; then OUT=$a; fi\n  prev=$a\ndone\nprintf '%s\\n' \"$@\" > {dir}/argv\nenv > {dir}/env\n{guion}"
    )
    .expect("escribir el guion");
    drop(f);
    Command::new("chmod")
        .arg("+x")
        .arg(&picker)
        .status()
        .expect("chmod");

    let ready = format!("{dir}/listo");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_eclipse-portald"));
    for (k, v) in env {
        cmd.env(k, v);
    }
    let portald = cmd
        .arg(format!("--address=unix:path={sock}"))
        .arg(format!("--picker={picker}"))
        .arg(format!("--answers={dir}/answers"))
        .arg(format!("--ready={ready}"))
        .spawn()
        .expect("arrancar eclipse-portald");
    // `--ready` is written only once the name is held, which is exactly the
    // guarantee the session needs before it exports GTK_USE_PORTAL=1.
    until("el fichero de --ready", || {
        std::path::Path::new(&ready).exists()
    });
    World { sock, dir, portald }
}

/// Call `OpenFile`/`SaveFile` and give back the handle the portal returned.
fn ask(c: &mut Client, member: &str, opts: Vec<(String, Arg)>) -> String {
    let r = c
        .call(
            PORTAL_NAME,
            PORTAL_PATH,
            IFACE_FILE_CHOOSER,
            member,
            &[
                Arg::Str(String::new()),
                Arg::Str("Abrir archivo".into()),
                Arg::DictSV(opts),
            ],
        )
        .unwrap_or_else(|e| panic!("{member}: {e}"));
    match r.args().first() {
        Some(Arg::Path(h)) | Some(Arg::Str(h)) => h.clone(),
        other => panic!("{member} no devolvio un handle: {other:?}"),
    }
}

/// The `Response` signal on `handle`, as `(code, uris)`.
fn response(c: &mut Client, handle: &str) -> (u32, Vec<String>) {
    let h = handle.to_string();
    let m = c
        .wait_for(|m| {
            m.kind == MSG_SIGNAL
                && m.member.as_deref() == Some("Response")
                && m.interface.as_deref() == Some(IFACE_REQUEST)
                && m.path.as_deref() == Some(h.as_str())
        })
        .expect("la senal Response");
    let args = m.args();
    let code = match args.first() {
        Some(Arg::U32(v)) => *v,
        other => panic!("el codigo no es u32: {other:?}"),
    };
    let uris = match args.get(1) {
        Some(Arg::DictSV(d)) => match d.iter().find(|(k, _)| k == "uris") {
            Some((_, Arg::StrArray(v))) => v.clone(),
            None => Vec::new(),
            other => panic!("uris no es as: {other:?}"),
        },
        other => panic!("los resultados no son a{{sv}}: {other:?}"),
    };
    (code, uris)
}

#[test]
fn the_portal_takes_its_name_and_answers_whoever_asks() {
    let w = start("exit 1");
    let mut c = Client::connect(&w.sock).expect("connect");
    // The name is what every client looks for; without it a GTK application
    // falls back to its own dialog and the portal might as well not exist.
    let r = c
        .call(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "GetNameOwner",
            &[Arg::Str(PORTAL_NAME.into())],
        )
        .expect("GetNameOwner");
    assert!(
        matches!(r.args().first(), Some(Arg::Str(s)) if s.starts_with(":1.")),
        "{:?}",
        r.args()
    );
    // Ping on any object: how a client checks the service is alive.
    c.call(PORTAL_NAME, "/", "org.freedesktop.DBus.Peer", "Ping", &[])
        .expect("Ping");
    // The version decides what the client may call.
    let v = c
        .call(
            PORTAL_NAME,
            PORTAL_PATH,
            "org.freedesktop.DBus.Properties",
            "Get",
            &[
                Arg::Str(IFACE_FILE_CHOOSER.into()),
                Arg::Str("version".into()),
            ],
        )
        .expect("Get version");
    assert_eq!(
        v.args().first(),
        Some(&Arg::Variant(Box::new(Arg::U32(
            eclipse_dbusd::portal::FILE_CHOOSER_VERSION
        ))))
    );
    // And it introspects, which is how a human finds out it is up.
    let x = c
        .call(
            PORTAL_NAME,
            PORTAL_PATH,
            "org.freedesktop.DBus.Introspectable",
            "Introspect",
            &[],
        )
        .expect("Introspect");
    match x.args().first() {
        Some(Arg::Str(xml)) => assert!(xml.contains("OpenFile"), "{xml}"),
        other => panic!("{other:?}"),
    }
    // A method it does not have is an error, not silence: silence is a client
    // waiting for ever.
    assert!(c
        .call(
            PORTAL_NAME,
            PORTAL_PATH,
            IFACE_FILE_CHOOSER,
            "SaveFiles",
            &[]
        )
        .is_err());
    assert!(c
        .call(
            PORTAL_NAME,
            "/no/existe",
            IFACE_FILE_CHOOSER,
            "OpenFile",
            &[]
        )
        .is_err());
}

#[test]
fn opening_a_file_gives_its_uri_back_to_the_client_that_asked() {
    // The chooser "chooses" two files, one of them with a space in its name:
    // the URI is what the client parses, and a raw space is a URI GTK refuses.
    let w = start("printf '%s\\n' '/root/a.txt' '/root/mis cosas/b.png' > \"$OUT\"");
    let mut c = Client::connect(&w.sock).expect("connect");
    let handle = ask(
        &mut c,
        "OpenFile",
        vec![
            ("handle_token".to_string(), Arg::Str("gtk1".into())),
            ("multiple".to_string(), Arg::Bool(true)),
            (
                "current_folder".to_string(),
                Arg::ByteArray(b"/root/mis cosas\0".to_vec()),
            ),
        ],
    );
    // The handle names this client and the token it chose.
    assert_eq!(
        handle,
        format!(
            "/org/freedesktop/portal/desktop/request/{}/gtk1",
            c.unique.trim_start_matches(':').replace('.', "_")
        )
    );
    let (code, uris) = response(&mut c, &handle);
    assert_eq!(code, RESPONSE_OK);
    assert_eq!(
        uris,
        vec!["file:///root/a.txt", "file:///root/mis%20cosas/b.png"]
    );
    // The options reached the chooser as arguments.
    let argv = std::fs::read_to_string(format!("{}/argv", w.dir)).expect("argv");
    let argv: Vec<&str> = argv.lines().collect();
    assert!(argv.contains(&"--pick"), "{argv:?}");
    assert!(argv.contains(&"--multiple"), "{argv:?}");
    assert!(argv.contains(&"--title"), "{argv:?}");
    assert!(argv.contains(&"Abrir archivo"), "{argv:?}");
    assert!(argv.contains(&"/root/mis cosas"), "{argv:?}");
    // The answer file is cleaned up: it names a file the user chose.
    let left: Vec<_> = std::fs::read_dir(format!("{}/answers", w.dir))
        .expect("answers")
        .filter_map(|e| e.ok())
        .collect();
    assert!(left.is_empty(), "quedo una respuesta sin borrar");
}

#[test]
fn a_cancelled_dialog_counts_as_cancelled_and_not_as_a_file() {
    // Nothing written and a non-zero exit: the user pressed Esc. What decides
    // is the FILE, not the status, because a chooser killed by a signal also
    // exits non-zero with a good answer already written.
    let w = start("exit 1");
    let mut c = Client::connect(&w.sock).expect("connect");
    let handle = ask(&mut c, "OpenFile", Vec::new());
    let (code, uris) = response(&mut c, &handle);
    assert_eq!(code, RESPONSE_CANCELLED);
    assert!(uris.is_empty(), "{uris:?}");
}

#[test]
fn an_answer_already_written_counts_even_if_the_dialog_dies_badly() {
    // The file is the answer. A chooser that wrote the path and then died
    // (killed, or crashed on the way out) has still been answered, and
    // throwing that away would lose the file the user picked.
    let w = start("printf '%s\\n' /root/elegido.txt > \"$OUT\"; kill -9 $$");
    let mut c = Client::connect(&w.sock).expect("connect");
    let handle = ask(&mut c, "OpenFile", Vec::new());
    let (code, uris) = response(&mut c, &handle);
    assert_eq!(code, RESPONSE_OK);
    assert_eq!(uris, vec!["file:///root/elegido.txt"]);
}

#[test]
fn a_save_passes_the_name_the_application_offered_and_comes_back_writable() {
    let w = start("printf '%s\\n' \"/root/nota.txt\" > \"$OUT\"");
    let mut c = Client::connect(&w.sock).expect("connect");
    let handle = ask(
        &mut c,
        "SaveFile",
        vec![
            ("handle_token".to_string(), Arg::Str("qt_0".into())),
            ("current_name".to_string(), Arg::Str("nota.txt".into())),
        ],
    );
    let m = c
        .wait_for(|m| m.kind == MSG_SIGNAL && m.member.as_deref() == Some("Response"))
        .expect("Response");
    assert_eq!(m.path.as_deref(), Some(handle.as_str()));
    match m.args().get(1) {
        Some(Arg::DictSV(d)) => {
            assert!(
                d.iter()
                    .any(|(k, v)| k == "writable" && *v == Arg::Bool(true)),
                "un Guardar vuelve escribible: {d:?}"
            );
        }
        other => panic!("{other:?}"),
    }
    let argv = std::fs::read_to_string(format!("{}/argv", w.dir)).expect("argv");
    let argv: Vec<&str> = argv.lines().collect();
    // `--save NAME`, never `--pick`: they are different dialogs.
    assert_eq!(argv[0], "--save", "{argv:?}");
    assert_eq!(argv[1], "nota.txt", "{argv:?}");
    assert!(!argv.contains(&"--pick"), "{argv:?}");
}

#[test]
fn a_token_that_climbs_out_of_its_place_never_reaches_the_signal() {
    // The token is pasted into an object path. A client that sends `../1_9/x`
    // must not end up with a handle in somebody else's directory, and the path
    // must stay legal: an illegal one is rejected by the bus, so the client
    // waits for a signal that can never arrive.
    let w = start("printf '%s\\n' /root/a > \"$OUT\"");
    let mut c = Client::connect(&w.sock).expect("connect");
    let handle = ask(
        &mut c,
        "OpenFile",
        vec![("handle_token".to_string(), Arg::Str("../1_9/x".into()))],
    );
    let mio = format!(
        "/org/freedesktop/portal/desktop/request/{}/",
        c.unique.trim_start_matches(':').replace('.', "_")
    );
    assert!(handle.starts_with(&mio), "{handle}");
    assert!(!handle.contains(".."), "{handle}");
    assert_eq!(handle[mio.len()..].matches('/').count(), 0, "{handle}");
    // And the signal arrives on it, which is what proves the path was valid.
    assert_eq!(response(&mut c, &handle).0, RESPONSE_OK);
}

#[test]
fn two_dialogs_at_once_do_not_tread_on_each_others_answer() {
    // The portal must keep serving while a dialog is up: a daemon that waited
    // for the first chooser would make the second client's call time out. The
    // script sleeps a little so both are open at once, and each answers with
    // its own answer file, which is the thing that must not be shared.
    let w = start("sleep 1; printf '%s\\n' \"$OUT\" > \"$OUT\"");
    let mut a = Client::connect(&w.sock).expect("connect a");
    let mut b = Client::connect(&w.sock).expect("connect b");
    let ha = ask(
        &mut a,
        "OpenFile",
        vec![("handle_token".to_string(), Arg::Str("a".into()))],
    );
    let hb = ask(
        &mut b,
        "OpenFile",
        vec![("handle_token".to_string(), Arg::Str("b".into()))],
    );
    assert_ne!(ha, hb, "dos clientes, dos handles");
    let (ca, ua) = response(&mut a, &ha);
    let (cb, ub) = response(&mut b, &hb);
    assert_eq!((ca, cb), (RESPONSE_OK, RESPONSE_OK));
    assert_eq!(ua.len(), 1);
    assert_eq!(ub.len(), 1);
    assert_ne!(ua, ub, "los dos dialogos escribieron en el mismo fichero");
}

#[test]
fn closing_a_request_takes_the_dialog_down_and_answers_nothing() {
    // `Close` means the client has stopped listening. A `Response` after it is
    // an answer to a question nobody asked any more, and libportal has already
    // dropped the handle.
    let w = start("sleep 30");
    let mut c = Client::connect(&w.sock).expect("connect");
    let handle = ask(&mut c, "OpenFile", Vec::new());
    c.call(PORTAL_NAME, &handle, IFACE_REQUEST, "Close", &[])
        .expect("Close");
    // Nothing comes back on that handle, and the portal is still serving: the
    // second call is the proof, because a daemon that died on `Close` would
    // simply never answer it.
    let mut c2 = Client::connect(&w.sock).expect("connect 2");
    c2.call(PORTAL_NAME, "/", "org.freedesktop.DBus.Peer", "Ping", &[])
        .expect("el portal sigue vivo");
    let quieto = c.poll(Duration::from_millis(300)).expect("poll");
    assert!(
        !matches!(&quieto, Some(m) if m.member.as_deref() == Some("Response")),
        "contesto despues de Close: {quieto:?}"
    );
}

#[test]
fn choosing_a_folder_is_refused_rather_than_invented() {
    // `lunarfile` navigates INTO a folder when one is activated, so there is
    // no gesture that means "this one". Handing back a file's parent would set
    // a download directory the user never pointed at; `RESPONSE_OTHER` (2) is
    // what the specification has for "the dialog could not be shown", and GTK
    // falls back to its own widget on it.
    let w = start("printf '%s\\n' /root/a > \"$OUT\"");
    let mut c = Client::connect(&w.sock).expect("connect");
    let handle = ask(
        &mut c,
        "OpenFile",
        vec![("directory".to_string(), Arg::Bool(true))],
    );
    let (code, uris) = response(&mut c, &handle);
    assert_eq!(code, 2);
    assert!(uris.is_empty());
    // And no dialog was opened for it: the script never ran.
    assert!(
        !std::path::Path::new(&format!("{}/argv", w.dir)).exists(),
        "abrio un dialogo para una carpeta"
    );
}

#[test]
fn a_second_portal_stands_aside_for_the_one_already_serving() {
    // Alpine's own xdg-desktop-portal may be installed, and it is the better
    // portal. Two owners of one name is a coin toss over which one serves every
    // call, so the second exits instead of queueing behind the first.
    let w = start("exit 1");
    // It is asked for a stamp on purpose: standing aside must not leave one
    // behind, since the session reads it as "a portal of OURS is serving".
    let stamp = format!("{}/listo2", w.dir);
    let out = Command::new(env!("CARGO_BIN_EXE_eclipse-portald"))
        .arg(format!("--address=unix:path={}", w.sock))
        .arg(format!("--answers={}/answers2", w.dir))
        .arg(format!("--ready={stamp}"))
        .output()
        .expect("segundo portald");
    assert!(
        !std::path::Path::new(&stamp).exists(),
        "el segundo portal dejo su sello sin servir nada"
    );
    assert!(out.status.success(), "{out:?}");
    let texto = String::from_utf8_lossy(&out.stdout);
    assert!(texto.contains("ya tiene dueno"), "{texto}");
    // The first one is still the owner and still answering.
    let mut c = Client::connect(&w.sock).expect("connect");
    c.call(PORTAL_NAME, "/", "org.freedesktop.DBus.Peer", "Ping", &[])
        .expect("Ping");
}

#[test]
fn no_portal_means_no_ready_file() {
    // The stamp is what the session reads to decide whether to export
    // GTK_USE_PORTAL=1. If it were written before the name was held, a session
    // whose portal failed would point every toolkit at a portal that is not
    // there -- which is worse than no portal at all, because the application's
    // own dialog is skipped.
    let dir = format!("/tmp/{}", unique("eclipse-portal-noready"));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let ready = format!("{dir}/listo");
    let out = Command::new(env!("CARGO_BIN_EXE_eclipse-portald"))
        .arg(format!("--address=unix:path={dir}/no-hay-bus"))
        .arg(format!("--answers={dir}/answers"))
        .arg(format!("--ready={ready}"))
        .output()
        .expect("portald sin bus");
    assert!(!out.status.success());
    assert!(
        !std::path::Path::new(&ready).exists(),
        "dijo que estaba listo sin bus"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_portal_leaves_when_the_bus_leaves() {
    // `eclipse-init` restarts it when the bus comes back; what matters is that
    // it does not sit there holding a dead connection, since a client's call
    // would then be answered by nobody. The bus is the real binary here, so
    // that it can be killed the way a crash would kill it.
    let dir = format!("/tmp/{}", unique("eclipse-portal-sinbus"));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let sock = format!("{dir}/bus");
    let mut bus = Command::new(env!("CARGO_BIN_EXE_eclipse-dbusd"))
        .arg("--session")
        .arg(format!("--address=unix:path={sock}"))
        .spawn()
        .expect("eclipse-dbusd");
    until("el bus escucha", || {
        std::os::unix::net::UnixStream::connect(&sock).is_ok()
    });
    let ready = format!("{dir}/listo");
    let mut portald = Command::new(env!("CARGO_BIN_EXE_eclipse-portald"))
        .arg(format!("--address=unix:path={sock}"))
        .arg(format!("--answers={dir}/answers"))
        .arg(format!("--ready={ready}"))
        .spawn()
        .expect("portald");
    until("el fichero de --ready", || {
        std::path::Path::new(&ready).exists()
    });
    let _ = bus.kill();
    let _ = bus.wait();
    until("que el portal se vaya", || {
        matches!(portald.try_wait(), Ok(Some(_)))
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// The dialog has to be started IN THE SESSION, and the portal is the one
/// process that cannot take that for granted.
///
/// `eclipse-init` gives every service `XDG_RUNTIME_DIR` and deliberately no
/// `WAYLAND_DISPLAY` (the compositor is what creates the socket, and it may
/// not have started yet). A chooser launched with that environment is a
/// Wayland client with no display: `lunarfile` prints one line, exits, writes
/// no answer -- and an answer with no paths in it is a CANCELLATION, so the
/// browser's "add files" does nothing at all and says nothing either. That was
/// the bug. The portal now resolves the socket when the dialog is asked for.
#[test]
fn the_chooser_is_started_in_the_session_even_with_none_in_the_environment() {
    let runtime = format!("/tmp/{}", unique("eclipse-portal-rt"));
    std::fs::create_dir_all(&runtime).expect("mkdir");
    // What a running labwc leaves there: the socket and its lock file.
    std::fs::write(format!("{runtime}/wayland-1"), "").expect("socket");
    std::fs::write(format!("{runtime}/wayland-1.lock"), "").expect("lock");
    // The portal starts with NO WAYLAND_DISPLAY, exactly as init starts it.
    let w = start_with_env(
        "printf '/tmp/x\\n' > \"$OUT\"",
        &[
            ("XDG_RUNTIME_DIR".to_string(), runtime.clone()),
            ("WAYLAND_DISPLAY".to_string(), String::new()),
        ],
    );
    let mut c = Client::connect(&w.sock).expect("connect");
    let handle = ask(&mut c, "OpenFile", vec![]);
    assert_eq!(response(&mut c, &handle).0, RESPONSE_OK);
    let env = std::fs::read_to_string(format!("{}/env", w.dir)).expect("el entorno del dialogo");
    let var = |k: &str| -> Option<String> {
        env.lines()
            .filter_map(|l| l.split_once('='))
            .find(|(n, _)| *n == k)
            .map(|(_, v)| v.to_string())
    };
    // The socket that is REALLY there, not the `wayland-0` a client guesses.
    assert_eq!(
        var("WAYLAND_DISPLAY").as_deref(),
        Some("wayland-1"),
        "{env}"
    );
    assert_eq!(
        var("XDG_RUNTIME_DIR").as_deref(),
        Some(runtime.as_str()),
        "{env}"
    );
    let _ = std::fs::remove_dir_all(&runtime);
}
