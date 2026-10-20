//! The logic of Eclipse's XDG desktop portal, apart from the socket.
//!
//! Why a portal exists at all
//! --------------------------
//! "Open a file" in a GTK application is not a GTK dialog any more. When
//! `GTK_USE_PORTAL=1` (and always inside a sandbox), GTK calls
//! `org.freedesktop.portal.FileChooser.OpenFile` on the session bus and waits
//! for a **`Response` signal on a request object** the portal hands back. Qt's
//! `QFileDialog`, Firefox's own `nsFilePicker` and anything built on `xdg-open`
//! go the same way. Eclipse had the two ends and nothing in between: a session
//! bus (`eclipse-dbusd`) and a file chooser (`lunarfile --pick`), with no
//! portal to introduce them. This module is that introduction.
//!
//! The shape of one call
//! ---------------------
//! ```text
//! -> OpenFile(s parent_window, s title, a{sv} options)   on /org/freedesktop/portal/desktop
//! <- (o handle)                                          immediately
//! ... the portal runs `lunarfile --pick --out FILE`
//! <- Response(u response, a{sv} results)                 signal on `handle`
//! ```
//! The **handle is returned before the dialog is even up**, and the client
//! blocks on the signal, not on the method. A portal that answered the method
//! only when the user had chosen would deadlock any client that waits for the
//! reply before it starts listening -- which is every one of them.
//!
//! Everything here is pure: a function of what came in off the bus. The socket,
//! the fork and the signal live in `src/bin/eclipse-portald.rs`, so the parts
//! that are easy to get wrong (an object path built out of a client-supplied
//! token, a path turned into a URI, a shell command line) can be tested without
//! a bus or a compositor.

use crate::message::Arg;

/// The well-known name every portal client asks for.
pub const PORTAL_NAME: &str = "org.freedesktop.portal.Desktop";
/// The one object that implements the portal interfaces.
pub const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
/// Where request handles live. A client never guesses it: it uses the path the
/// method returned. The layout is the one `xdg-desktop-portal` uses, and it is
/// kept because `libportal` reconstructs it to subscribe to the signal BEFORE
/// the method returns, to close the race with a dialog the user answers at once.
pub const REQUEST_PREFIX: &str = "/org/freedesktop/portal/desktop/request";

pub const IFACE_FILE_CHOOSER: &str = "org.freedesktop.portal.FileChooser";
pub const IFACE_REQUEST: &str = "org.freedesktop.portal.Request";
pub const IFACE_PROPERTIES: &str = "org.freedesktop.DBus.Properties";
pub const IFACE_INTROSPECTABLE: &str = "org.freedesktop.DBus.Introspectable";
pub const IFACE_PEER: &str = "org.freedesktop.DBus.Peer";

/// The `version` property of `org.freedesktop.portal.FileChooser`. 3 is the
/// version that added `SaveFiles`, which this portal does NOT implement, so it
/// announces 2: a client reads this number to decide what it may call, and
/// claiming a version whose methods are missing turns a graceful fallback into
/// an unexplained error.
pub const FILE_CHOOSER_VERSION: u32 = 2;

/// `Response` codes (`org.freedesktop.portal.Request`).
pub const RESPONSE_OK: u32 = 0;
/// The user cancelled the dialog.
pub const RESPONSE_CANCELLED: u32 = 1;
/// The dialog could not be shown, or ended some other way.
pub const RESPONSE_OTHER: u32 = 2;

/// The most paths one answer may carry: `lunarfile` will not mark more than
/// 1024 either, and the number bounds what one client can make the portal
/// allocate out of a file it does not own.
pub const MAX_PICKED: usize = 1024;

/// Make `token` safe to put in an object path.
///
/// The handle token comes straight from the client (`handle_token` in the
/// options) and is pasted into an object path. Two things go wrong if it is
/// taken as given. A token with a `/` in it climbs the tree: `handle_token` of
/// `../1_9/gtk1` would make this portal emit another client's `Response`, which
/// is one application cancelling another's dialog -- or reading the path it
/// chose. And a token with any other character outside `[A-Za-z0-9_]` is simply
/// not a valid object path, so the message is rejected by the bus and the
/// client waits for a signal that can never arrive: an "Open file" that hangs
/// for ever rather than failing.
///
/// Every byte outside the allowed set becomes `_`, and an empty token gets a
/// name of its own rather than an empty path element (`//`, also invalid).
pub fn sanitize_token(token: &str) -> String {
    let out: String = token
        .chars()
        .take(64)
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        "eclipse".to_string()
    } else {
        out
    }
}

/// The request object for `sender`'s `token`.
///
/// The sender's unique name is folded the way the portal specification says:
/// the leading `:` is dropped and every `.` becomes `_`, so `:1.17` is `1_17`.
/// That is what makes the path unique per client, and it is why one client
/// cannot name another's request: the sender is the bus's own stamp on the
/// message, not something the client writes.
pub fn request_path(sender: &str, token: &str) -> String {
    let who: String = sender
        .trim_start_matches(':')
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let who = if who.is_empty() {
        "anon".to_string()
    } else {
        who
    };
    format!("{REQUEST_PREFIX}/{who}/{}", sanitize_token(token))
}

/// Is `p` a well-formed D-Bus object path? The bus rejects a message whose
/// `PATH` is not, and a rejected `Response` is a client that waits for ever.
pub fn valid_object_path(p: &str) -> bool {
    if p == "/" {
        return true;
    }
    if !p.starts_with('/') || p.ends_with('/') || p.contains("//") {
        return false;
    }
    p.bytes()
        .all(|b| b == b'/' || b == b'_' || b.is_ascii_alphanumeric())
}

/// The value of `key` in an options dictionary.
pub fn opt<'a>(opts: &'a [(String, Arg)], key: &str) -> Option<&'a Arg> {
    opts.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

/// `key` as a boolean. A missing key, or one whose value is not a boolean, is
/// `false`: the options are optional by design, and every flag the file chooser
/// takes means "do the unusual thing" when set.
pub fn opt_bool(opts: &[(String, Arg)], key: &str) -> bool {
    matches!(opt(opts, key), Some(Arg::Bool(true)))
}

/// `key` as text. Both spellings are accepted because the portal uses both: the
/// labels are `s` and the paths are `ay` (NUL-terminated), since a filesystem
/// path need not be UTF-8.
pub fn opt_text(opts: &[(String, Arg)], key: &str) -> Option<String> {
    match opt(opts, key)? {
        Arg::Str(s) => Some(s.clone()),
        Arg::ByteArray(b) => path_from_ay(b),
        _ => None,
    }
}

/// A path out of an `ay`: the trailing NUL is not part of it.
///
/// Dropping only ONE trailing NUL matters: GTK sends the C string including its
/// terminator, and a path that kept it would be a different path -- `open` on
/// `"/root\0"` is `ENOENT`, which looks like the folder having been deleted.
pub fn path_from_ay(bytes: &[u8]) -> Option<String> {
    let end = bytes.strip_suffix(b"\0").unwrap_or(bytes);
    let s = String::from_utf8(end.to_vec()).ok()?;
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// What one `OpenFile`/`SaveFile` call asks for, once the options are read.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Ask {
    /// `SaveFile`: the answer is a path that need not exist yet.
    pub save: bool,
    /// `multiple`: several files, one per line.
    pub multiple: bool,
    /// `directory`: a folder rather than a file. Not implemented; see
    /// [`Ask::unsupported`].
    pub directory: bool,
    /// The dialog's title, as the application wrote it (already translated).
    pub title: String,
    /// Where to open, from `current_folder`.
    pub folder: Option<String>,
    /// The name to offer, from `current_name`.
    pub name: Option<String>,
}

impl Ask {
    /// Why this ask cannot be served, if it cannot.
    ///
    /// Only `directory` is refused, and it is refused rather than approximated:
    /// `lunarfile` navigates INTO a folder when one is activated, so there is
    /// no gesture that means "this one". Answering with a file's parent would
    /// hand the application a folder the user never pointed at -- a download
    /// directory silently set to somewhere else. `RESPONSE_OTHER` is what the
    /// specification has for "the dialog could not be shown", and a GTK client
    /// falls back to its own widget on it.
    pub fn unsupported(&self) -> Option<&'static str> {
        if self.directory {
            Some("lunarfile no sabe elegir carpetas todavia")
        } else {
            None
        }
    }
}

/// Read one `FileChooser` call. `None` for a member this portal does not
/// implement (`SaveFiles`, which takes a list of names and has no dialog here).
pub fn ask_from(member: &str, title: &str, opts: &[(String, Arg)]) -> Option<Ask> {
    let save = match member {
        "OpenFile" => false,
        "SaveFile" => true,
        _ => return None,
    };
    Some(Ask {
        save,
        // A Save dialog has one answer by construction, so `multiple` next to
        // `SaveFile` is a client's mistake and is ignored rather than passed
        // on: `lunarfile` refuses `--save` with `--multiple` outright, and the
        // refusal would come back as a dialog that never opened.
        multiple: !save && opt_bool(opts, "multiple"),
        directory: opt_bool(opts, "directory"),
        title: title.trim().to_string(),
        folder: opt_text(opts, "current_folder"),
        name: opt_text(opts, "current_name").or_else(|| {
            // `current_file` is a whole path; its last element is the name.
            let path = opt_text(opts, "current_file")?;
            Some(path.rsplit('/').next()?.to_string()).filter(|s| !s.is_empty())
        }),
    })
}

/// Quote `s` for `/bin/sh -c`.
///
/// The portal hands a command line to a shell, and every piece of it comes from
/// a client: a title from Firefox, a folder from GTK, a filename the user typed
/// into another application. A file innocently named
/// `No sé qué' ; reboot ; '.txt` offered as `current_name` is a reboot the
/// moment the dialog opens. Single quotes make everything literal; the only
/// character that needs care is the quote itself, closed and re-opened as
/// `'\''`.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The command line that shows the dialog and writes the answer to `out`.
///
/// `--out` rather than standard output: the chooser is started detached (its
/// parent is a daemon that must keep serving the bus while the dialog is up),
/// so there is no pipe to read. The file is the hand-off, and it is the portal
/// that chose its name.
pub fn picker_command(exe: &str, ask: &Ask, out: &str) -> String {
    let mut cmd = String::from(exe);
    if ask.save {
        // `--save NAME` is what puts `lunarfile` in Save mode, so a Save with
        // no name offered still needs the flag: an empty name is a field the
        // user types into.
        cmd.push_str(" --save ");
        cmd.push_str(&shell_quote(ask.name.as_deref().unwrap_or("")));
    } else {
        cmd.push_str(" --pick");
        if ask.multiple {
            cmd.push_str(" --multiple");
        }
    }
    if let Some(dir) = &ask.folder {
        cmd.push_str(" --dir ");
        cmd.push_str(&shell_quote(dir));
    }
    if !ask.title.is_empty() {
        cmd.push_str(" --title ");
        cmd.push_str(&shell_quote(&ask.title));
    }
    cmd.push_str(" --out ");
    cmd.push_str(&shell_quote(out));
    cmd
}

/// `file://` URI for an absolute path, percent-encoding everything that is not
/// allowed unescaped.
///
/// The portal answers with URIs, never with paths, and GTK parses them with
/// `g_filename_from_uri`. A space left raw there is a URI the client rejects,
/// so a file called `mis cosas.txt` would simply not open; a `#` left raw is
/// worse, because the part after it is dropped as a fragment and the client
/// opens a file that does not exist. `/` is kept (it is the separator) and so
/// are the unreserved characters `-_.~`; the rest goes out as `%XX` per BYTE of
/// UTF-8, which is what the RFC asks for.
pub fn file_uri(path: &str) -> String {
    let mut out = String::from("file://");
    for b in path.bytes() {
        let keep = b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.' | b'~');
        if keep {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    out
}

/// The body of a `Response` signal: `(u response, a{sv} results)`.
///
/// `uris` is present only on success. An empty `uris` next to `RESPONSE_OK`
/// would be a client told "the user chose" with nothing to open, and GTK turns
/// that into an empty filename rather than a cancellation.
pub fn response_body(code: u32, uris: &[String], writable: bool) -> Vec<Arg> {
    let mut results: Vec<(String, Arg)> = Vec::new();
    if code == RESPONSE_OK && !uris.is_empty() {
        results.push((
            "uris".to_string(),
            Arg::StrArray(uris.iter().map(|p| file_uri(p)).collect()),
        ));
        if writable {
            // A Save answer is a file the application is about to write.
            results.push(("writable".to_string(), Arg::Bool(true)));
        }
    }
    vec![Arg::U32(code), Arg::DictSV(results)]
}

/// The paths `lunarfile` wrote: one per line, absolute.
///
/// Anything else is dropped rather than passed on. A relative line cannot have
/// come from `lunarfile` (it prints absolute paths), so it is either a truncated
/// write or a file somebody else put at that name, and a relative path handed to
/// a client would be resolved against ITS working directory -- a different file
/// entirely.
pub fn parse_picked(text: &str) -> Vec<String> {
    text.lines()
        .map(|l| l.trim_end_matches(['\r', '\n']))
        .filter(|l| l.starts_with('/'))
        .take(MAX_PICKED)
        .map(|l| l.to_string())
        .collect()
}

/// The XML for `org.freedesktop.DBus.Introspectable.Introspect`.
///
/// GTK does not need it, but `gdbus introspect` is how a human finds out
/// whether the portal is up, and `busctl`-shaped tooling refuses to call a
/// method it cannot see. The request object answers with its own, shorter
/// document: a request has no properties and one method.
pub fn introspection(path: &str) -> String {
    let head = "<!DOCTYPE node PUBLIC \"-//freedesktop//DTD D-BUS Object Introspection 1.0//EN\" \
                \"http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd\">\n<node>\n  \
                <interface name=\"org.freedesktop.DBus.Introspectable\">\n    \
                <method name=\"Introspect\"><arg name=\"xml\" type=\"s\" direction=\"out\"/>\
                </method>\n  </interface>\n";
    if path == PORTAL_PATH {
        format!(
            "{head}  <interface name=\"org.freedesktop.DBus.Properties\">\n    \
             <method name=\"Get\"><arg type=\"s\" direction=\"in\"/><arg type=\"s\" \
             direction=\"in\"/><arg type=\"v\" direction=\"out\"/></method>\n    \
             <method name=\"GetAll\"><arg type=\"s\" direction=\"in\"/><arg type=\"a{{sv}}\" \
             direction=\"out\"/></method>\n  </interface>\n  \
             <interface name=\"{IFACE_FILE_CHOOSER}\">\n    \
             <method name=\"OpenFile\"><arg name=\"parent_window\" type=\"s\" direction=\"in\"/>\
             <arg name=\"title\" type=\"s\" direction=\"in\"/><arg name=\"options\" \
             type=\"a{{sv}}\" direction=\"in\"/><arg name=\"handle\" type=\"o\" \
             direction=\"out\"/></method>\n    \
             <method name=\"SaveFile\"><arg name=\"parent_window\" type=\"s\" direction=\"in\"/>\
             <arg name=\"title\" type=\"s\" direction=\"in\"/><arg name=\"options\" \
             type=\"a{{sv}}\" direction=\"in\"/><arg name=\"handle\" type=\"o\" \
             direction=\"out\"/></method>\n    \
             <property name=\"version\" type=\"u\" access=\"read\"/>\n  </interface>\n</node>\n"
        )
    } else {
        format!(
            "{head}  <interface name=\"{IFACE_REQUEST}\">\n    \
             <method name=\"Close\"/>\n    \
             <signal name=\"Response\"><arg name=\"response\" type=\"u\"/>\
             <arg name=\"results\" type=\"a{{sv}}\"/></signal>\n  </interface>\n</node>\n"
        )
    }
}

/// What one incoming method call is asking for. The daemon's loop is a switch
/// on this and nothing else, so the decision table can be tested without a bus.
#[derive(Clone, Debug, PartialEq)]
pub enum Call {
    /// `FileChooser.OpenFile` or `FileChooser.SaveFile`, by member name.
    Choose(String),
    /// `Properties.Get` of the `version` property.
    Version,
    /// `Properties.GetAll`.
    AllProperties,
    /// `Introspectable.Introspect`, on whichever object was named.
    Introspect,
    /// `Peer.Ping`.
    Ping,
    /// `Peer.GetMachineId`.
    MachineId,
    /// `Request.Close` on a request handle, which the daemon has to look up.
    CloseRequest,
    /// The path is not one this portal serves.
    NoSuchObject,
    /// The path is right and the member is not.
    NoSuchMethod,
}

/// Is `path` one of the request handles this portal hands out? Only the shape
/// is checked here; whether THIS portal issued it is a lookup the daemon does.
pub fn is_request_path(path: &str) -> bool {
    path.len() > REQUEST_PREFIX.len() + 1 && path.starts_with(&format!("{REQUEST_PREFIX}/"))
}

/// Classify an incoming method call.
///
/// A call with no interface is legal on the wire (the field is optional for
/// method calls) and real clients send it: `Ping` without an interface is what
/// `dbus-send --peer` does. It is resolved by member, which is unambiguous for
/// the handful this portal answers.
pub fn classify(path: &str, iface: Option<&str>, member: Option<&str>) -> Call {
    let member = match member {
        Some(m) => m,
        None => return Call::NoSuchMethod,
    };
    // `Peer` answers on EVERY object, including ones that do not exist: that is
    // what the specification says, and it is how a client checks the service is
    // alive before it asks for anything.
    if iface == Some(IFACE_PEER) || (iface.is_none() && matches!(member, "Ping" | "GetMachineId")) {
        return match member {
            "Ping" => Call::Ping,
            "GetMachineId" => Call::MachineId,
            _ => Call::NoSuchMethod,
        };
    }
    if path == PORTAL_PATH {
        return match (iface, member) {
            (Some(IFACE_INTROSPECTABLE), "Introspect") | (None, "Introspect") => Call::Introspect,
            (Some(IFACE_PROPERTIES), "Get") => Call::Version,
            (Some(IFACE_PROPERTIES), "GetAll") => Call::AllProperties,
            (Some(IFACE_FILE_CHOOSER), m @ ("OpenFile" | "SaveFile"))
            | (None, m @ ("OpenFile" | "SaveFile")) => Call::Choose(m.to_string()),
            _ => Call::NoSuchMethod,
        };
    }
    if is_request_path(path) {
        return match (iface, member) {
            (Some(IFACE_INTROSPECTABLE), "Introspect") | (None, "Introspect") => Call::Introspect,
            (Some(IFACE_REQUEST), "Close") | (None, "Close") => Call::CloseRequest,
            _ => Call::NoSuchMethod,
        };
    }
    Call::NoSuchObject
}

/// The name of the file the chooser writes its answer to.
///
/// It is the portal that picks the name, inside a directory only the portal
/// writes to: the answer decides which file an application opens, so a name a
/// client could predict (or supply) would be a name it could plant. The counter
/// keeps two dialogs of the same second apart.
pub fn answer_file(dir: &str, pid: u32, nanos: u128, seq: u64) -> String {
    format!("{dir}/pick-{pid}-{nanos:x}-{seq}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// Run `cmd` through a real `/bin/sh` and hand back its standard output.
    /// The point is that the quoting is checked by the thing that will do the
    /// unquoting, not by a second opinion in Rust.
    fn sh(cmd: &str) -> String {
        let out = Command::new("/bin/sh")
            .arg("-c")
            .arg(cmd)
            .output()
            .expect("/bin/sh");
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    /// A one-off directory under the scratch area, named after this process.
    fn scratch(tag: &str) -> String {
        let dir = format!(
            "/tmp/eclipse-portal-test-{}-{tag}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    #[test]
    fn a_handle_token_cannot_escape_its_own_request_path() {
        // The token comes from the client and is pasted into an object path. A
        // `/` in it would climb the tree: `../1_9/gtk1` emits ANOTHER client's
        // Response, which is one application cancelling another's dialog and
        // reading the path it chose.
        assert_eq!(sanitize_token("gtk1"), "gtk1");
        assert_eq!(sanitize_token("t_42"), "t_42");
        assert_eq!(sanitize_token("../1_9/gtk1"), "___1_9_gtk1");
        assert_eq!(sanitize_token("a/b"), "a_b");
        assert_eq!(sanitize_token("a.b"), "a_b");
        assert_eq!(sanitize_token("a b"), "a_b");
        assert_eq!(sanitize_token("ñ"), "_");
        // An empty token is not an empty path element: `//` is not a path.
        assert_eq!(sanitize_token(""), "eclipse");
        // And it is bounded, because the path goes in every message.
        assert_eq!(sanitize_token(&"x".repeat(500)).len(), 64);
        // Whatever the client sends, the path it produces is a legal one.
        for nasty in [
            "",
            "/",
            "..",
            "../..",
            "a/../../b",
            "con espacios",
            "coma,punto.",
            "\0nulo",
            "\n",
            "ünïcode",
            "'; reboot ;'",
            &"y".repeat(300),
        ] {
            let p = request_path(":1.17", nasty);
            assert!(valid_object_path(&p), "{nasty:?} -> {p}");
            assert!(
                p.starts_with(&format!("{REQUEST_PREFIX}/1_17/")),
                "{nasty:?} -> {p}"
            );
            // Exactly one element below the client's own directory.
            assert_eq!(p[REQUEST_PREFIX.len() + 1..].matches('/').count(), 1, "{p}");
        }
    }

    #[test]
    fn a_request_path_names_the_client_the_bus_stamped() {
        // `:1.17` folds to `1_17`: the leading colon goes and the dots become
        // underscores. It is the bus's own SENDER field, so one client cannot
        // name another's request.
        assert_eq!(
            request_path(":1.17", "gtk1"),
            "/org/freedesktop/portal/desktop/request/1_17/gtk1"
        );
        assert_ne!(
            request_path(":1.17", "gtk1"),
            request_path(":1.18", "gtk1"),
            "dos clientes con el mismo token comparten objeto"
        );
        // A well-known name as sender (the bus never sends one, but a monitor
        // or a test can) still gives a legal path.
        let p = request_path("org.example.App", "t");
        assert!(valid_object_path(&p), "{p}");
        // No sender at all: a path rather than `//`.
        assert!(valid_object_path(&request_path("", "t")));
        assert!(request_path("", "t").contains("/anon/"));
    }

    #[test]
    fn an_object_path_is_what_the_specification_says_it_is() {
        for good in [
            "/",
            "/a",
            "/org/freedesktop/portal/desktop",
            "/org/freedesktop/portal/desktop/request/1_17/gtk1",
            "/_",
            "/a0",
        ] {
            assert!(valid_object_path(good), "{good}");
        }
        for bad in [
            "", "a", "a/b", "/a/", "/a//b", "/a.b", "/a-b", "/a b", "/ñ", "/a:b",
        ] {
            assert!(!valid_object_path(bad), "{bad}");
        }
    }

    #[test]
    fn an_option_is_read_by_name_and_by_type() {
        let opts = vec![
            ("multiple".to_string(), Arg::Bool(true)),
            ("directory".to_string(), Arg::Bool(false)),
            ("accept_label".to_string(), Arg::Str("Abrir".into())),
            (
                "current_folder".to_string(),
                Arg::ByteArray(b"/root/mis cosas\0".to_vec()),
            ),
            ("mentira".to_string(), Arg::U32(1)),
        ];
        assert!(opt_bool(&opts, "multiple"));
        assert!(!opt_bool(&opts, "directory"));
        // Missing, and present with the wrong type, are both "no".
        assert!(!opt_bool(&opts, "no_esta"));
        assert!(!opt_bool(&opts, "mentira"));
        assert_eq!(opt_text(&opts, "accept_label").as_deref(), Some("Abrir"));
        // A path arrives as `ay`, NUL-terminated, and comes out as a path.
        assert_eq!(
            opt_text(&opts, "current_folder").as_deref(),
            Some("/root/mis cosas")
        );
        assert_eq!(opt_text(&opts, "no_esta"), None);
        assert_eq!(opt_text(&opts, "mentira"), None);
        assert_eq!(opt(&opts, "multiple"), Some(&Arg::Bool(true)));
        assert_eq!(opt(&opts, "no_esta"), None);
    }

    #[test]
    fn a_path_in_a_byte_array_loses_its_terminator_and_nothing_else() {
        // GTK sends the C string WITH its NUL. Keeping it makes every `open`
        // fail with ENOENT, which reads as the folder having been deleted.
        assert_eq!(path_from_ay(b"/root\0").as_deref(), Some("/root"));
        // Only one, and only at the end: a path is allowed to be odd, and
        // trimming more would invent a different one.
        assert_eq!(path_from_ay(b"/root\0\0").as_deref(), Some("/root\0"));
        assert_eq!(path_from_ay(b"/root").as_deref(), Some("/root"));
        assert_eq!(path_from_ay("/café\0".as_bytes()).as_deref(), Some("/café"));
        // Nothing is not a folder, and neither is a lone terminator.
        assert_eq!(path_from_ay(b""), None);
        assert_eq!(path_from_ay(b"\0"), None);
        // A path that is not UTF-8 cannot be put on a command line here, so it
        // is refused rather than mangled into a different path.
        assert_eq!(path_from_ay(&[b'/', 0xff, 0xfe]), None);
    }

    #[test]
    fn each_file_chooser_method_becomes_the_ask_it_means() {
        let open = ask_from("OpenFile", " Abrir archivo ", &[]).expect("OpenFile");
        assert_eq!(
            open,
            Ask {
                save: false,
                multiple: false,
                directory: false,
                // The title is trimmed: it is drawn in the dialog's header.
                title: "Abrir archivo".to_string(),
                folder: None,
                name: None,
            }
        );
        assert_eq!(open.unsupported(), None);

        let many = ask_from("OpenFile", "", &[("multiple".to_string(), Arg::Bool(true))])
            .expect("OpenFile");
        assert!(many.multiple && !many.save);

        let save = ask_from(
            "SaveFile",
            "Guardar",
            &[
                ("current_name".to_string(), Arg::Str("nota.txt".into())),
                (
                    "current_folder".to_string(),
                    Arg::ByteArray(b"/tmp\0".to_vec()),
                ),
                // A Save that also asks for several files is the client's
                // mistake: `lunarfile` refuses the pair outright, which would
                // come back as a dialog that never opened.
                ("multiple".to_string(), Arg::Bool(true)),
            ],
        )
        .expect("SaveFile");
        assert!(save.save);
        assert!(!save.multiple, "un Guardar no puede ser multiple");
        assert_eq!(save.name.as_deref(), Some("nota.txt"));
        assert_eq!(save.folder.as_deref(), Some("/tmp"));

        // `current_file` is a whole path and only its last element is a name.
        let from_file = ask_from(
            "SaveFile",
            "",
            &[(
                "current_file".to_string(),
                Arg::ByteArray(b"/root/docs/informe.odt\0".to_vec()),
            )],
        )
        .expect("SaveFile");
        assert_eq!(from_file.name.as_deref(), Some("informe.odt"));
        // `current_name` wins when both are there: it is the more specific.
        let both = ask_from(
            "SaveFile",
            "",
            &[
                ("current_name".to_string(), Arg::Str("elegido".into())),
                (
                    "current_file".to_string(),
                    Arg::ByteArray(b"/root/otro.txt\0".to_vec()),
                ),
            ],
        )
        .expect("SaveFile");
        assert_eq!(both.name.as_deref(), Some("elegido"));
        // A path that ends in a slash has no name in it.
        let trailing = ask_from(
            "SaveFile",
            "",
            &[(
                "current_file".to_string(),
                Arg::ByteArray(b"/root/docs/\0".to_vec()),
            )],
        )
        .expect("SaveFile");
        assert_eq!(trailing.name, None);

        // Choosing a FOLDER is refused, not approximated: handing back a
        // file's parent sets a download directory the user never pointed at.
        let dir = ask_from(
            "OpenFile",
            "",
            &[("directory".to_string(), Arg::Bool(true))],
        )
        .expect("OpenFile");
        assert!(dir.directory);
        assert!(dir.unsupported().is_some());

        // And a method this portal does not implement is not guessed at.
        assert_eq!(ask_from("SaveFiles", "", &[]), None);
        assert_eq!(ask_from("OpenURI", "", &[]), None);
        assert_eq!(ask_from("", "", &[]), None);
    }

    #[test]
    fn a_filename_that_is_a_shell_command_stays_a_filename() {
        // The dialog is launched through `/bin/sh -c`, and every piece of the
        // command line comes from a client: a title from Firefox, a folder from
        // GTK, a name the user typed somewhere else. `/bin/sh` itself is the
        // judge here, which is the only opinion that counts.
        for raw in [
            "simple.txt",
            "con espacios.txt",
            "No sé qué' ; reboot ; '.txt",
            "$(reboot)",
            "`reboot`",
            "a\"b",
            "a\\b",
            "a$HOME",
            "a*b?c[d]",
            "a&b|c;d",
            "a\nb",
            "¡ñandú!",
            "--out",
        ] {
            let got = sh(&format!("printf %s {}", shell_quote(raw)));
            assert_eq!(got, raw, "shell_quote({raw:?}) = {}", shell_quote(raw));
        }
        // Nothing is still an argument, and an empty one at that.
        assert_eq!(sh(&format!("printf '[%s]' {}", shell_quote(""))), "[]");
    }

    #[test]
    fn the_dialog_is_started_with_the_arguments_the_ask_means() {
        // The command line is built as text and torn apart by a shell, so what
        // is checked is the argv the chooser actually receives. The stand-in
        // prints its own arguments, one per line.
        let dir = scratch("argv");
        let prog = format!("{dir}/fake-lunarfile");
        std::fs::write(&prog, "#!/bin/sh\nprintf '%s\\n' \"$@\"\n").expect("write");
        sh(&format!("chmod +x {}", shell_quote(&prog)));

        let argv = |ask: &Ask, out: &str| -> Vec<String> {
            sh(&picker_command(&prog, ask, out))
                .lines()
                .map(|l| l.to_string())
                .collect()
        };

        // An Open is `--pick`, and the answer file is always last.
        let open = Ask {
            title: "Abrir archivo".into(),
            ..Default::default()
        };
        assert_eq!(
            argv(&open, "/run/a"),
            vec!["--pick", "--title", "Abrir archivo", "--out", "/run/a"]
        );
        // Several files add `--multiple`, and nothing else changes.
        let many = Ask {
            multiple: true,
            ..open.clone()
        };
        assert_eq!(
            argv(&many, "/run/a"),
            vec![
                "--pick",
                "--multiple",
                "--title",
                "Abrir archivo",
                "--out",
                "/run/a"
            ]
        );
        // A Save is `--save NAME`, never `--pick`: the two are different
        // dialogs and `lunarfile` refuses to be both.
        let save = Ask {
            save: true,
            name: Some("nota.txt".into()),
            folder: Some("/root/mis cosas".into()),
            title: "Guardar como".into(),
            ..Default::default()
        };
        assert_eq!(
            argv(&save, "/run/b"),
            vec![
                "--save",
                "nota.txt",
                "--dir",
                "/root/mis cosas",
                "--title",
                "Guardar como",
                "--out",
                "/run/b"
            ]
        );
        // A Save with no name offered still needs the flag, and the empty name
        // has to survive as an argument of its own -- dropping it would make
        // `--save` eat the next flag.
        let bare = Ask {
            save: true,
            ..Default::default()
        };
        assert_eq!(argv(&bare, "/run/c"), vec!["--save", "", "--out", "/run/c"]);
        // No title: no flag, rather than an empty header.
        let untitled = Ask::default();
        assert_eq!(argv(&untitled, "/run/d"), vec!["--pick", "--out", "/run/d"]);

        // And the whole point: a hostile name reaches the chooser as one
        // argument and runs nothing on the way.
        let mean = Ask {
            save: true,
            name: Some("No sé qué' ; touch ".to_string() + &dir + "/pwned ; '.txt"),
            title: "$(touch ".to_string() + &dir + "/pwned2)",
            ..Default::default()
        };
        let got = argv(&mean, "/run/e");
        assert_eq!(got.len(), 6, "{got:?}");
        assert_eq!(got[1], mean.name.clone().unwrap());
        assert_eq!(got[3], mean.title);
        assert!(!std::path::Path::new(&format!("{dir}/pwned")).exists());
        assert!(!std::path::Path::new(&format!("{dir}/pwned2")).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_path_becomes_a_uri_a_client_can_parse() {
        assert_eq!(file_uri("/root/a.txt"), "file:///root/a.txt");
        // A space left raw is a URI GTK refuses, so the file simply does not
        // open; a `#` left raw is worse, because everything after it is dropped
        // as a fragment and the client opens a file that is not there.
        assert_eq!(
            file_uri("/root/mis cosas/b.png"),
            "file:///root/mis%20cosas/b.png"
        );
        assert_eq!(file_uri("/root/a#b.txt"), "file:///root/a%23b.txt");
        assert_eq!(file_uri("/root/a?b"), "file:///root/a%3Fb");
        assert_eq!(file_uri("/root/100%.txt"), "file:///root/100%25.txt");
        // The unreserved characters stay as they are: escaping them is legal
        // but noisy, and `~` in particular reads as a home directory.
        assert_eq!(file_uri("/a-b_c.d~e/F9"), "file:///a-b_c.d~e/F9");
        // Non-ASCII goes out per BYTE of UTF-8, which is what the RFC asks and
        // what `g_filename_from_uri` undoes.
        assert_eq!(file_uri("/ñ"), "file:///%C3%B1");
        assert_eq!(file_uri("/€"), "file:///%E2%82%AC");
        // Upper-case hex, as the RFC prefers, and never a raw byte above 0x7f.
        for p in ["/ñ", "/€", "/a b", "/a\u{7f}"] {
            let u = file_uri(p);
            assert!(u.is_ascii(), "{u}");
            assert!(!u[7..].contains(' '), "{u}");
        }
    }

    #[test]
    fn the_response_body_says_what_happened_and_nothing_else() {
        // Success carries the URIs. `writable` marks a Save, which is a file
        // the application is about to write.
        let ok = response_body(RESPONSE_OK, &["/root/a b.txt".to_string()], false);
        assert_eq!(ok[0], Arg::U32(0));
        assert_eq!(
            ok[1],
            Arg::DictSV(vec![(
                "uris".to_string(),
                Arg::StrArray(vec!["file:///root/a%20b.txt".to_string()])
            )])
        );
        let saved = response_body(RESPONSE_OK, &["/root/a".to_string()], true);
        match &saved[1] {
            Arg::DictSV(d) => {
                assert_eq!(d.len(), 2, "{d:?}");
                assert_eq!(d[1], ("writable".to_string(), Arg::Bool(true)));
            }
            other => panic!("{other:?}"),
        }
        // A cancellation carries NOTHING: an empty `uris` next to a success
        // code is a client told "the user chose" with nothing to open, and GTK
        // turns that into an empty filename instead of a cancellation.
        for (code, uris) in [
            (RESPONSE_CANCELLED, &["/root/a".to_string()][..]),
            (RESPONSE_OK, &[][..]),
            (RESPONSE_OTHER, &[][..]),
        ] {
            let b = response_body(code, uris, true);
            assert_eq!(b[0], Arg::U32(code));
            assert_eq!(b[1], Arg::DictSV(Vec::new()), "codigo {code}");
        }
        // Several files keep their order: the application pairs them with its
        // own list.
        let many = response_body(
            RESPONSE_OK,
            &["/a".to_string(), "/b".to_string(), "/c".to_string()],
            false,
        );
        match &many[1] {
            Arg::DictSV(d) => assert_eq!(
                d[0].1,
                Arg::StrArray(vec![
                    "file:///a".to_string(),
                    "file:///b".to_string(),
                    "file:///c".to_string()
                ])
            ),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn only_absolute_paths_come_out_of_the_answer_file() {
        assert_eq!(
            parse_picked("/root/a.txt\n/root/b.txt\n"),
            vec!["/root/a.txt", "/root/b.txt"]
        );
        // No trailing newline is still a path: the chooser was killed after the
        // write but before the flush, and the path it wrote is good.
        assert_eq!(parse_picked("/root/a.txt"), vec!["/root/a.txt"]);
        assert_eq!(parse_picked("/root/a.txt\r\n"), vec!["/root/a.txt"]);
        // A relative line cannot have come from `lunarfile`, and handed to a
        // client it would resolve against ITS working directory: a different
        // file.
        assert_eq!(parse_picked("a.txt\n/root/b\n"), vec!["/root/b"]);
        assert_eq!(parse_picked("../etc/shadow\n"), Vec::<String>::new());
        assert_eq!(parse_picked(""), Vec::<String>::new());
        assert_eq!(parse_picked("\n\n"), Vec::<String>::new());
        // A name with a space in it is one path, not two.
        assert_eq!(
            parse_picked("/root/mis cosas/a b\n"),
            vec!["/root/mis cosas/a b"]
        );
        // And the number of them is bounded: the file is read, not trusted.
        let many: String = (0..MAX_PICKED + 50).map(|i| format!("/f{i}\n")).collect();
        assert_eq!(parse_picked(&many).len(), MAX_PICKED);
    }

    #[test]
    fn every_call_this_portal_answers_is_told_apart_from_the_rest() {
        let req = request_path(":1.9", "gtk1");
        let cases: &[(&str, Option<&str>, Option<&str>, Call)] = &[
            (
                PORTAL_PATH,
                Some(IFACE_FILE_CHOOSER),
                Some("OpenFile"),
                Call::Choose("OpenFile".into()),
            ),
            (
                PORTAL_PATH,
                Some(IFACE_FILE_CHOOSER),
                Some("SaveFile"),
                Call::Choose("SaveFile".into()),
            ),
            // `SaveFiles` exists in version 3 of the interface and not here, so
            // it must come back as a missing method and not as a Save.
            (
                PORTAL_PATH,
                Some(IFACE_FILE_CHOOSER),
                Some("SaveFiles"),
                Call::NoSuchMethod,
            ),
            (
                PORTAL_PATH,
                Some(IFACE_PROPERTIES),
                Some("Get"),
                Call::Version,
            ),
            (
                PORTAL_PATH,
                Some(IFACE_PROPERTIES),
                Some("GetAll"),
                Call::AllProperties,
            ),
            (
                PORTAL_PATH,
                Some(IFACE_PROPERTIES),
                Some("Set"),
                Call::NoSuchMethod,
            ),
            (
                PORTAL_PATH,
                Some(IFACE_INTROSPECTABLE),
                Some("Introspect"),
                Call::Introspect,
            ),
            (&req, Some(IFACE_REQUEST), Some("Close"), Call::CloseRequest),
            (
                &req,
                Some(IFACE_INTROSPECTABLE),
                Some("Introspect"),
                Call::Introspect,
            ),
            // The FileChooser lives on the portal object and nowhere else.
            (
                &req,
                Some(IFACE_FILE_CHOOSER),
                Some("OpenFile"),
                Call::NoSuchMethod,
            ),
            (
                PORTAL_PATH,
                Some(IFACE_REQUEST),
                Some("Close"),
                Call::NoSuchMethod,
            ),
            // Another portal interface this daemon does not implement.
            (
                PORTAL_PATH,
                Some("org.freedesktop.portal.Screenshot"),
                Some("Screenshot"),
                Call::NoSuchMethod,
            ),
            // An object nobody serves.
            (
                "/",
                Some(IFACE_FILE_CHOOSER),
                Some("OpenFile"),
                Call::NoSuchObject,
            ),
            (
                "/org/freedesktop/portal",
                Some(IFACE_INTROSPECTABLE),
                Some("Introspect"),
                Call::NoSuchObject,
            ),
            // The request prefix itself is not a handle.
            (
                REQUEST_PREFIX,
                Some(IFACE_REQUEST),
                Some("Close"),
                Call::NoSuchObject,
            ),
            // `Peer` answers on any object at all, which is how a client finds
            // out the service is alive before it asks for anything.
            ("/", Some(IFACE_PEER), Some("Ping"), Call::Ping),
            (PORTAL_PATH, Some(IFACE_PEER), Some("Ping"), Call::Ping),
            (
                "/lo/que/sea",
                Some(IFACE_PEER),
                Some("GetMachineId"),
                Call::MachineId,
            ),
            ("/", Some(IFACE_PEER), Some("Otro"), Call::NoSuchMethod),
            // The interface field is optional in a method call, and real tools
            // leave it out.
            (
                PORTAL_PATH,
                None,
                Some("OpenFile"),
                Call::Choose("OpenFile".into()),
            ),
            (PORTAL_PATH, None, Some("Introspect"), Call::Introspect),
            ("/", None, Some("Ping"), Call::Ping),
            (&req, None, Some("Close"), Call::CloseRequest),
            // No member at all is not a call.
            (
                PORTAL_PATH,
                Some(IFACE_FILE_CHOOSER),
                None,
                Call::NoSuchMethod,
            ),
        ];
        for (path, iface, member, want) in cases {
            assert_eq!(
                &classify(path, *iface, *member),
                want,
                "{path} {iface:?} {member:?}"
            );
        }
    }

    #[test]
    fn the_request_prefix_alone_is_not_a_request() {
        assert!(is_request_path(&request_path(":1.1", "t")));
        assert!(is_request_path(&format!("{REQUEST_PREFIX}/x")));
        assert!(!is_request_path(REQUEST_PREFIX));
        assert!(!is_request_path(&format!("{REQUEST_PREFIX}/")));
        assert!(!is_request_path(PORTAL_PATH));
        assert!(!is_request_path(
            "/org/freedesktop/portal/desktop/requests/x"
        ));
    }

    #[test]
    fn the_introspection_names_the_methods_a_client_will_call() {
        let x = introspection(PORTAL_PATH);
        for want in [
            "OpenFile",
            "SaveFile",
            IFACE_FILE_CHOOSER,
            "name=\"version\"",
            "a{sv}",
            "<node>",
            "</node>",
        ] {
            assert!(x.contains(want), "falta {want} en\n{x}");
        }
        // The braces of `a{sv}` survive the formatting: `a{{sv}}` is what a
        // forgotten escape leaves behind, and no client parses that.
        assert!(!x.contains("a{{sv}}"), "{x}");
        // `SaveFiles` is version 3 and is not implemented, so it is not
        // advertised either.
        assert!(!x.contains("SaveFiles"), "{x}");

        // A request object has one method and one signal, and no properties.
        let r = introspection(&request_path(":1.1", "t"));
        assert!(r.contains(IFACE_REQUEST), "{r}");
        assert!(r.contains("name=\"Close\""), "{r}");
        assert!(r.contains("name=\"Response\""), "{r}");
        assert!(!r.contains("OpenFile"), "{r}");
        assert!(!r.contains("<property"), "{r}");
        // Both documents are introspectable themselves, which is what makes
        // `gdbus introspect --recurse` work at all.
        for doc in [&x, &r] {
            assert!(doc.contains(IFACE_INTROSPECTABLE), "{doc}");
            assert_eq!(doc.matches("<node>").count(), 1, "{doc}");
            assert_eq!(
                doc.matches("<interface").count(),
                doc.matches("</interface>").count(),
                "{doc}"
            );
        }
    }

    #[test]
    fn the_version_announced_is_one_whose_methods_all_exist() {
        // A client reads this number to decide what it may call. Announcing 3
        // promises `SaveFiles`, which is not here, and turns a graceful
        // fallback into an unexplained error.
        assert_eq!(FILE_CHOOSER_VERSION, 2);
        assert!(!introspection(PORTAL_PATH).contains("SaveFiles"));
    }

    #[test]
    fn two_dialogs_never_share_an_answer_file() {
        // The answer decides which file an application opens, so two dialogs
        // up at once must not write to one name.
        let a = answer_file("/run/p", 10, 1_000, 1);
        let b = answer_file("/run/p", 10, 1_000, 2);
        let c = answer_file("/run/p", 11, 1_000, 1);
        let d = answer_file("/run/p", 10, 1_001, 1);
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d);
        for f in [&a, &b, &c, &d] {
            assert!(f.starts_with("/run/p/"), "{f}");
            // One element inside the directory: a name, not a subtree.
            assert_eq!(f["/run/p/".len()..].matches('/').count(), 0, "{f}");
        }
    }

    #[test]
    fn an_option_key_is_matched_whole_and_its_value_is_taken_as_it_came() {
        // The options dictionary is the client's: it may carry keys this
        // portal has never heard of, from a newer revision of the
        // specification or from another implementation. A key that merely
        // EXTENDS one of ours must not be read as ours, and the flags are
        // where that bites, because every one of them means "do the unusual
        // thing" when set. `directory` refuses the dialog outright, so a
        // client that sent `directory_hint` would get no dialog at all.
        let longer = vec![
            ("directory_hint".to_string(), Arg::Bool(true)),
            ("multiple_files".to_string(), Arg::Bool(true)),
            (
                "current_folder_uri".to_string(),
                Arg::Str("/root/otro".into()),
            ),
        ];
        assert!(!opt_bool(&longer, "directory"), "a longer key is not ours");
        assert!(!opt_bool(&longer, "multiple"));
        assert_eq!(opt_text(&longer, "current_folder"), None);
        let ask = ask_from("OpenFile", "t", &longer).unwrap();
        assert_eq!(ask.unsupported(), None, "the dialog is still served");
        assert_eq!((ask.multiple, ask.folder), (false, None));

        // And the two `current_` keys that look alike are told apart, since
        // one is a path and the other the filter the dialog starts with.
        let opts = vec![
            ("current_filter".to_string(), Arg::Str("Imagenes".into())),
            (
                "current_file".to_string(),
                Arg::ByteArray(b"/root/foto.png\0".to_vec()),
            ),
        ];
        assert_eq!(
            opt_text(&opts, "current_file").as_deref(),
            Some("/root/foto.png")
        );
        assert_eq!(
            ask_from("SaveFile", "t", &opts).unwrap().name.as_deref(),
            Some("foto.png")
        );

        // A text value is whatever the client sent, byte for byte. A filename
        // may legitimately begin or end in a space, and normalising one away
        // offers the application a name that names a different file -- or, for
        // a folder, opens somewhere else entirely.
        let spaced = vec![
            ("current_name".to_string(), Arg::Str(" notas.txt ".into())),
            (
                "current_folder".to_string(),
                Arg::ByteArray(b"/root/mis cosas \0".to_vec()),
            ),
        ];
        let ask = ask_from("SaveFile", "t", &spaced).unwrap();
        assert_eq!(ask.name.as_deref(), Some(" notas.txt "));
        assert_eq!(ask.folder.as_deref(), Some("/root/mis cosas "));
    }

    #[test]
    fn an_answer_path_with_a_space_in_it_still_names_one_file() {
        // `--answers=DIR` is an operator's flag, so a directory with a space in
        // it is ordinary. Unquoted, the command line splits there: `lunarfile`
        // writes its answer to the first half and the portal waits on a file
        // nobody wrote, which is an "Open file" that hangs for ever rather
        // than failing.
        let out = "/run/user/1000/eclipse portal/pick-7-1a-0";
        let cmd = picker_command("lunarfile", &Ask::default(), out);
        // Asked of the shell that will do the unquoting, not of a second
        // opinion in Rust.
        assert_eq!(sh(&format!("set -- {cmd}; echo $#")).trim(), "4");
        assert_eq!(
            sh(&format!("set -- {cmd}; printf '%s' \"$4\"")),
            out,
            "{cmd}"
        );
    }

    #[test]
    fn a_byte_below_sixteen_is_escaped_with_two_digits_and_not_one() {
        // A tab or a newline is legal in a filename, and `%9` is not an escape:
        // the client reads the character after it as part of the number, so
        // `/a<TAB>b` arrives as `/a` plus the byte 0x9b -- a different path,
        // and usually one that is not there. The test above covers 0x7f, which
        // needs no padding, so this is the case that was missing.
        assert_eq!(file_uri("/a\tb"), "file:///a%09b");
        assert_eq!(file_uri("/a\nb"), "file:///a%0Ab");
        assert_eq!(file_uri("/a\u{1}b"), "file:///a%01b");
        // Every escape is three characters wide, for every byte that needs one.
        for b in 1u8..=0x0f {
            let u = file_uri(&format!("/x{}y", b as char));
            assert_eq!(u.len(), "file:///xy".len() + 3, "{u}");
        }
    }

    #[test]
    fn a_picked_path_keeps_its_own_spaces_and_loses_only_the_line_ending() {
        // A filename may end in a space or a tab: trimming them hands the
        // client a path that names another file, or nothing at all.
        assert_eq!(parse_picked("/root/notas.txt \n"), ["/root/notas.txt "]);
        assert_eq!(parse_picked("/root/a\tb\n"), ["/root/a\tb"]);
        // A plain CRLF file does NOT exercise the trim: `str::lines` already
        // takes `\r\n` as one line ending. What the trim is for is a write
        // that stopped in the middle of one, which is what a chooser killed
        // between the two bytes leaves behind -- and a path with a `\r` glued
        // to its end is as wrong as one with a space eaten off it.
        assert_eq!(
            parse_picked("/root/a.txt\r\n/root/b.txt\r\n"),
            ["/root/a.txt", "/root/b.txt"]
        );
        assert_eq!(parse_picked("/root/a.txt\r"), ["/root/a.txt"]);
        assert_eq!(parse_picked("/root/a.txt\r\r\n"), ["/root/a.txt"]);
        // And whitespace alone is still not an absolute path.
        assert!(parse_picked(" \n\t\n").is_empty());
    }

    #[test]
    fn the_version_property_is_declared_the_way_it_is_answered() {
        // A typed client builds its call out of this document: `gdbus` and any
        // generated proxy read the signature from here, and the access mode
        // with it. Declaring `s` where a `u` comes back is a client that fails
        // to demarshal the reply it asked for.
        //
        // `u` is D-Bus for u32, and it is spelled through the variant the
        // portal actually sends so that changing the constant's type stops
        // compiling here instead of quietly disagreeing.
        let sig = match Arg::U32(FILE_CHOOSER_VERSION) {
            Arg::U32(_) => "u",
            _ => unreachable!(),
        };
        let xml = introspection(PORTAL_PATH);
        assert!(
            xml.contains(&format!(
                "<property name=\"version\" type=\"{sig}\" access=\"read\"/>"
            )),
            "{xml}"
        );
        // And `read` is all there is: a `Set` on this interface is not a method
        // this portal has, so advertising the property as writable would
        // advertise a call that can only come back as an error.
        assert_eq!(
            classify(PORTAL_PATH, Some(IFACE_PROPERTIES), Some("Set")),
            Call::NoSuchMethod
        );
    }

    #[test]
    fn the_three_response_codes_are_the_ones_the_specification_numbers() {
        // These travel to the client as bare numbers and GTK switches on them.
        // 1 is "the user cancelled", which shows no error and retries nothing;
        // 2 is "the dialog could not be shown", which makes a GTK client fall
        // back to its own widget. A cancellation numbered 2 turns "the user
        // said no" into "the portal is broken", and an error numbered 1 hides
        // a portal that cannot work at all. They are not ours to choose: the
        // numbers are the specification's, which is why they are pinned here
        // rather than left to read out of the constants.
        assert_eq!(
            (RESPONSE_OK, RESPONSE_CANCELLED, RESPONSE_OTHER),
            (0u32, 1u32, 2u32)
        );
    }
}
