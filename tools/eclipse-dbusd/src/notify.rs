//! The logic of Eclipse's notification server, apart from the socket.
//!
//! What it is for
//! --------------
//! A program that wants to tell the user something ("the download finished",
//! "new mail", "the build failed") does not draw a window: it calls
//! `org.freedesktop.Notifications.Notify` on the session bus, and whatever owns
//! that name puts a card on the screen. libnotify (and so `notify-send`), GLib's
//! `GNotification` outside a portal, Firefox, Qt's `QSystemTrayIcon` fallback and
//! every terminal's "command finished" hint all go that way. Eclipse had the two
//! ends and nothing in between: a session bus (`eclipse-dbusd`) and the cards the
//! bar already draws for hunter's alerts (`lunarbar::hunter_notify`, a spool
//! directory of one small file per alert). This module is the translation.
//!
//! The shape of one notification
//! -----------------------------
//! ```text
//! -> Notify(s app, u replaces, s icon, s summary, s body, as actions, a{sv} hints, i timeout)
//! <- (u id)                                           at once
//! ... the alert file goes into the spool; the bar shows the card
//! ... the user presses a button, closes it, or it expires; the bar answers
//! <- ActionInvoked(u id, s key)                       when a button was pressed
//! <- NotificationClosed(u id, u reason)               always, last
//! ```
//!
//! Everything here is pure: a function of what came in off the bus or out of
//! the spool. The loop lives in `src/bin/eclipse-notifyd.rs`.

use crate::message::Arg;

/// The well-known name every notification client calls.
pub const NAME: &str = "org.freedesktop.Notifications";
/// The one object, and its interface.
pub const PATH: &str = "/org/freedesktop/Notifications";
pub const IFACE: &str = "org.freedesktop.Notifications";

/// Where the bar reads alerts from. Kept in step with
/// `lunarbar::hunter_notify::SPOOL` by a test on the bar's side.
pub const SPOOL: &str = "/run/hunter/notify";

/// `NotificationClosed` reasons, as the specification numbers them.
pub const CLOSED_EXPIRED: u32 = 1;
pub const CLOSED_DISMISSED: u32 = 2;
pub const CLOSED_BY_CALL: u32 = 3;

/// Buttons a card has room for; the rest of a client's actions are dropped.
pub const MAX_ACTIONS: usize = 3;
/// A summary or body longer than this many characters is cut. The bar cuts
/// again to what fits; this is a wall against a client sending a megabyte.
const MAX_TEXT: usize = 512;

/// One notification as the bar will show it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Note {
    pub app: String,
    pub summary: String,
    pub body: String,
    /// `(key the client gave, label)`, at most [`MAX_ACTIONS`].
    pub actions: Vec<(String, String)>,
    /// `info` or `crit`, from the `urgency` hint.
    pub critical: bool,
    /// Seconds on screen: `None` for the bar's default, `Some(0)` for until
    /// the user closes it.
    pub timeout: Option<u32>,
    /// The id of the notification this one replaces, 0 for none.
    pub replaces: u32,
}

/// Text off the bus, made safe for one line of an alert file: no markup, no
/// control characters (a newline would start a new key), capped.
///
/// The specification lets a server advertise `body-markup` and then receive a
/// small subset of HTML. This server does not advertise it, but clients send
/// `<b>` and `&amp;` anyway, and a card that shows the tags is worse than one
/// that drops them.
pub fn clean(text: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if in_tag => {}
            '&' => {
                let mut ent = String::new();
                while let Some(&n) = chars.peek() {
                    if n == ';' || ent.len() > 6 {
                        break;
                    }
                    ent.push(n);
                    chars.next();
                }
                let known = match ent.as_str() {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" | "#39" => Some('\''),
                    _ => None,
                };
                match (known, chars.peek()) {
                    (Some(k), Some(';')) => {
                        chars.next();
                        out.push(k);
                    }
                    _ => {
                        out.push('&');
                        out.push_str(&ent);
                    }
                }
            }
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    let collapsed = out.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(MAX_TEXT).collect()
}

/// The arguments of a `Notify` call, or None when they are not its shape.
pub fn note_from(args: &[Arg]) -> Option<Note> {
    let s = |i: usize| match args.get(i) {
        Some(Arg::Str(s)) => Some(s.as_str()),
        _ => None,
    };
    let app = s(0)?;
    let replaces = match args.get(1) {
        Some(Arg::U32(v)) => *v,
        _ => return None,
    };
    let summary = s(3)?;
    let body = s(4)?;
    let actions = match args.get(5) {
        Some(Arg::StrArray(a)) => a.clone(),
        _ => Vec::new(),
    };
    let hints = match args.get(6) {
        Some(Arg::DictSV(d)) => d.clone(),
        _ => Vec::new(),
    };
    // `i`: -1 is "the server's default", 0 is "never", otherwise milliseconds.
    // The reader hands an INT32 back as its bits in a U32.
    let timeout = match args.get(7) {
        Some(Arg::U32(v)) => {
            let ms = *v as i32;
            match ms {
                0 => Some(0),
                ms if ms > 0 => Some((ms as u32).div_ceil(1000)),
                _ => None,
            }
        }
        _ => None,
    };
    let critical = hints
        .iter()
        .any(|(k, v)| k == "urgency" && matches!(v, Arg::Byte(2) | Arg::U32(2)));
    let mut pairs = Vec::new();
    for pair in actions.chunks(2) {
        if let [key, label] = pair {
            let label = clean(label);
            // The `default` action is what a click on the card body means; a
            // client often gives it an empty label, and a button with no text
            // is no button. It is shown as "Abrir", which is what it does.
            let label = if label.is_empty() && key == "default" {
                "Abrir".to_string()
            } else {
                label
            };
            if !key.is_empty() && !label.is_empty() && pairs.len() < MAX_ACTIONS {
                pairs.push((key.clone(), label));
            }
        }
    }
    let app = clean(app);
    let summary = clean(summary);
    Some(Note {
        // A notification with no summary still has something to say: the
        // card's title is what the bar cannot do without, so the program's
        // name stands in.
        summary: if summary.is_empty() {
            app.clone()
        } else {
            summary
        },
        app,
        body: clean(body),
        actions: pairs,
        critical,
        // A critical notification stays until it is dealt with, as the
        // specification asks, unless the client said how long.
        timeout: if critical && timeout.is_none() {
            Some(0)
        } else {
            timeout
        },
        replaces,
    })
}

/// The spool's action id for the button at `index`. Client keys can be
/// anything at all, and the spool's ids are a file-name-safe alphabet, so the
/// key stays here and the file carries its position.
pub fn action_id(index: usize) -> String {
    format!("a{index}")
}

/// The client's key for an answer the bar wrote, if it was a button.
pub fn key_of<'a>(note: &'a Note, answer: &str) -> Option<&'a str> {
    let index: usize = answer.strip_prefix('a')?.parse().ok()?;
    note.actions.get(index).map(|(k, _)| k.as_str())
}

/// How the notification closed, from the bar's answer.
pub fn closed_reason(answer: &str) -> u32 {
    match answer {
        "timeout" => CLOSED_EXPIRED,
        _ => CLOSED_DISMISSED,
    }
}

/// The alert file for `note`, in the format `lunarbar::hunter_notify::parse`
/// reads. `reply=1` always: this server has to tell the client how the card
/// went, buttons or not.
pub fn render(note: &Note) -> String {
    let severity = if note.critical { "crit" } else { "info" };
    let mut s = format!("severity={severity}\ntitle={}\n", note.summary);
    if !note.app.is_empty() {
        s += &format!("app={}\n", note.app);
    }
    if !note.body.is_empty() {
        s += &format!("body={}\n", note.body);
    }
    if let Some(t) = note.timeout {
        s += &format!("timeout={t}\n");
    }
    s += "reply=1\n";
    for (i, (_, label)) in note.actions.iter().enumerate() {
        s += &format!("action={}\t{label}\n", action_id(i));
    }
    s
}

/// What `GetServerInformation` answers.
pub fn server_information() -> [Arg; 4] {
    [
        Arg::Str("Eclipse".into()),
        Arg::Str("Eclipse OS".into()),
        Arg::Str(env!("CARGO_PKG_VERSION").into()),
        Arg::Str("1.2".into()),
    ]
}

/// What `GetCapabilities` answers: buttons and a plain body. Not
/// `body-markup` (the markup is dropped, see [`clean`]), not `icon-static`
/// (the card has no icon slot), not `persistence` (the cards go when closed).
pub fn capabilities() -> Arg {
    Arg::StrArray(vec!["actions".into(), "body".into()])
}

/// The introspection XML of the one object.
pub fn introspection() -> String {
    format!(
        "<!DOCTYPE node PUBLIC \"-//freedesktop//DTD D-BUS Object Introspection 1.0//EN\" \
         \"http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd\">\n<node>\n  \
         <interface name=\"org.freedesktop.DBus.Introspectable\">\n    \
         <method name=\"Introspect\"><arg name=\"xml\" type=\"s\" direction=\"out\"/></method>\n  \
         </interface>\n  <interface name=\"{IFACE}\">\n    \
         <method name=\"Notify\"><arg name=\"app_name\" type=\"s\" direction=\"in\"/>\
         <arg name=\"replaces_id\" type=\"u\" direction=\"in\"/>\
         <arg name=\"app_icon\" type=\"s\" direction=\"in\"/>\
         <arg name=\"summary\" type=\"s\" direction=\"in\"/>\
         <arg name=\"body\" type=\"s\" direction=\"in\"/>\
         <arg name=\"actions\" type=\"as\" direction=\"in\"/>\
         <arg name=\"hints\" type=\"a{{sv}}\" direction=\"in\"/>\
         <arg name=\"expire_timeout\" type=\"i\" direction=\"in\"/>\
         <arg name=\"id\" type=\"u\" direction=\"out\"/></method>\n    \
         <method name=\"CloseNotification\"><arg name=\"id\" type=\"u\" direction=\"in\"/></method>\n    \
         <method name=\"GetCapabilities\"><arg name=\"capabilities\" type=\"as\" direction=\"out\"/></method>\n    \
         <method name=\"GetServerInformation\"><arg name=\"name\" type=\"s\" direction=\"out\"/>\
         <arg name=\"vendor\" type=\"s\" direction=\"out\"/>\
         <arg name=\"version\" type=\"s\" direction=\"out\"/>\
         <arg name=\"spec_version\" type=\"s\" direction=\"out\"/></method>\n    \
         <signal name=\"NotificationClosed\"><arg name=\"id\" type=\"u\"/><arg name=\"reason\" type=\"u\"/></signal>\n    \
         <signal name=\"ActionInvoked\"><arg name=\"id\" type=\"u\"/><arg name=\"action_key\" type=\"s\"/></signal>\n  \
         </interface>\n</node>\n"
    )
}

/// What a method call on this server is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Call {
    Notify,
    Close,
    Capabilities,
    ServerInformation,
    Introspect,
    Ping,
    NoSuchObject,
    NoSuchMethod,
}

/// Classify an incoming method call. As with the portal, a call without an
/// interface is legal and is matched on the member alone.
pub fn classify(path: &str, iface: Option<&str>, member: Option<&str>) -> Call {
    let Some(member) = member else {
        return Call::NoSuchMethod;
    };
    if member == "Ping" && matches!(iface, None | Some("org.freedesktop.DBus.Peer")) {
        return Call::Ping;
    }
    if path != PATH {
        return Call::NoSuchObject;
    }
    match (iface, member) {
        (None | Some("org.freedesktop.DBus.Introspectable"), "Introspect") => Call::Introspect,
        (None | Some(IFACE), "Notify") => Call::Notify,
        (None | Some(IFACE), "CloseNotification") => Call::Close,
        (None | Some(IFACE), "GetCapabilities") => Call::Capabilities,
        (None | Some(IFACE), "GetServerInformation") => Call::ServerInformation,
        _ => Call::NoSuchMethod,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notify_args(
        app: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: Vec<(String, Arg)>,
        ms: i32,
    ) -> Vec<Arg> {
        vec![
            Arg::Str(app.into()),
            Arg::U32(0),
            Arg::Str(String::new()),
            Arg::Str(summary.into()),
            Arg::Str(body.into()),
            Arg::StrArray(actions.iter().map(|s| s.to_string()).collect()),
            Arg::DictSV(hints),
            Arg::U32(ms as u32),
        ]
    }

    #[test]
    fn a_plain_notification_becomes_an_alert_the_bar_reads() {
        let n = note_from(&notify_args(
            "Firefox",
            "Descarga completada",
            "informe.pdf",
            &[],
            vec![],
            -1,
        ))
        .unwrap();
        assert_eq!(n.app, "Firefox");
        assert_eq!(n.summary, "Descarga completada");
        assert_eq!(n.timeout, None);
        assert!(!n.critical);
        let file = render(&n);
        assert!(
            file.starts_with("severity=info\ntitle=Descarga completada\n"),
            "{file}"
        );
        assert!(file.contains("app=Firefox\n"));
        assert!(file.contains("body=informe.pdf\n"));
        assert!(file.contains("reply=1\n"));
        assert!(!file.contains("timeout="));
    }

    #[test]
    fn the_alert_file_is_exactly_what_the_bar_s_test_reads() {
        // `lunarbar::hunter_notify`'s test parses these very lines; the two
        // crates share the format and not the code, so both ends pin it.
        let n = note_from(&notify_args(
            "Firefox",
            "Descarga completada",
            "informe.pdf (2,1 MiB)",
            &["default", "", "open", "Abrir carpeta"],
            vec![],
            8000,
        ))
        .unwrap();
        assert_eq!(
            render(&n),
            "severity=info\ntitle=Descarga completada\napp=Firefox\n\
             body=informe.pdf (2,1 MiB)\ntimeout=8\nreply=1\n\
             action=a0\tAbrir\naction=a1\tAbrir carpeta\n"
        );
    }

    #[test]
    fn the_timeout_is_milliseconds_minus_one_is_default_and_zero_is_never() {
        let t = |ms| {
            note_from(&notify_args("a", "s", "", &[], vec![], ms))
                .unwrap()
                .timeout
        };
        assert_eq!(t(-1), None);
        assert_eq!(t(0), Some(0));
        assert_eq!(t(5000), Some(5));
        // Rounded up: a 1.2 s notification must not vanish after one second.
        assert_eq!(t(1200), Some(2));
        assert_eq!(t(1), Some(1));
    }

    #[test]
    fn critical_urgency_stays_until_closed_unless_the_client_says_otherwise() {
        let crit = vec![("urgency".to_string(), Arg::Byte(2))];
        let n = note_from(&notify_args("a", "s", "", &[], crit.clone(), -1)).unwrap();
        assert!(n.critical);
        assert_eq!(n.timeout, Some(0));
        assert!(render(&n).starts_with("severity=crit\n"));
        let n = note_from(&notify_args("a", "s", "", &[], crit, 3000)).unwrap();
        assert_eq!(n.timeout, Some(3));
        let low = vec![("urgency".to_string(), Arg::Byte(0))];
        assert!(
            !note_from(&notify_args("a", "s", "", &[], low, -1))
                .unwrap()
                .critical
        );
    }

    #[test]
    fn actions_become_buttons_keyed_by_position_and_are_capped() {
        let n = note_from(&notify_args(
            "a",
            "s",
            "",
            &[
                "default",
                "",
                "open-folder",
                "Abrir carpeta",
                "x y/z",
                "Rara",
                "4",
                "Cuarta",
            ],
            vec![],
            -1,
        ))
        .unwrap();
        assert_eq!(
            n.actions,
            vec![
                ("default".to_string(), "Abrir".to_string()),
                ("open-folder".to_string(), "Abrir carpeta".to_string()),
                ("x y/z".to_string(), "Rara".to_string()),
            ]
        );
        let file = render(&n);
        assert!(file.contains("action=a0\tAbrir\n"));
        assert!(file.contains("action=a2\tRara\n"));
        assert_eq!(key_of(&n, "a1"), Some("open-folder"));
        assert_eq!(key_of(&n, "a2"), Some("x y/z"));
        assert_eq!(key_of(&n, "a9"), None);
        assert_eq!(key_of(&n, "dismiss"), None);
        // An odd trailing key with no label is not a button.
        let odd = note_from(&notify_args("a", "s", "", &["solo"], vec![], -1)).unwrap();
        assert!(odd.actions.is_empty());
    }

    #[test]
    fn markup_and_newlines_never_reach_the_alert_file() {
        assert_eq!(
            clean("<b>Hola</b> &amp; adi&oacute;s"),
            "Hola & adi&oacute;s"
        );
        assert_eq!(clean("uno\ndos\r\ntres"), "uno dos tres");
        assert_eq!(
            clean("1 &lt; 2 &gt; 0 &quot;x&quot; &apos;"),
            "1 < 2 > 0 \"x\" '"
        );
        let n = note_from(&notify_args(
            "a",
            "linea\ninyectada",
            "x\nreply=0\nseverity=crit",
            &[],
            vec![],
            -1,
        ))
        .unwrap();
        let file = render(&n);
        assert_eq!(
            file.lines().filter(|l| l.starts_with("severity=")).count(),
            1,
            "{file}"
        );
        assert!(file.contains("title=linea inyectada\n"));
        assert!(clean(&"x".repeat(10_000)).len() <= 512);
    }

    #[test]
    fn a_notification_with_no_summary_is_titled_by_its_program() {
        let n = note_from(&notify_args("Terminal", "", "listo", &[], vec![], -1)).unwrap();
        assert_eq!(n.summary, "Terminal");
    }

    #[test]
    fn a_call_of_the_wrong_shape_is_refused() {
        assert_eq!(note_from(&[]), None);
        assert_eq!(
            note_from(&[Arg::Str("a".into()), Arg::Str("no".into())]),
            None
        );
    }

    #[test]
    fn the_bar_s_answers_map_to_the_specification_s_reasons() {
        assert_eq!(closed_reason("timeout"), CLOSED_EXPIRED);
        assert_eq!(closed_reason("dismiss"), CLOSED_DISMISSED);
        assert_eq!(closed_reason("a0"), CLOSED_DISMISSED);
    }

    #[test]
    fn calls_are_classified_with_or_without_an_interface() {
        assert_eq!(classify(PATH, Some(IFACE), Some("Notify")), Call::Notify);
        assert_eq!(classify(PATH, None, Some("CloseNotification")), Call::Close);
        assert_eq!(
            classify(PATH, Some(IFACE), Some("GetCapabilities")),
            Call::Capabilities
        );
        assert_eq!(
            classify(PATH, Some(IFACE), Some("GetServerInformation")),
            Call::ServerInformation
        );
        assert_eq!(classify(PATH, None, Some("Introspect")), Call::Introspect);
        assert_eq!(classify("/otro", None, Some("Ping")), Call::Ping);
        assert_eq!(
            classify("/otro", Some(IFACE), Some("Notify")),
            Call::NoSuchObject
        );
        assert_eq!(
            classify(PATH, Some(IFACE), Some("Nada")),
            Call::NoSuchMethod
        );
        assert!(introspection().contains("NotificationClosed"));
    }
}
