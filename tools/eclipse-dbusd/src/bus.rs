//! Bus state: connections, name ownership, match rules and routing.
//!
//! Deliberately transport-free. [`Bus`] takes decoded messages and produces
//! decoded messages in [`Bus::outbox`]; sockets, polling and file descriptors
//! all live in `main.rs`. That split is what makes the routing rules testable
//! on the host without a running daemon — and the routing rules are where a
//! message bus is either correct or subtly, silently wrong.

use std::collections::BTreeMap;

use crate::message::{Arg, MatchRule, Message, FLAG_NO_REPLY_EXPECTED, MSG_METHOD_CALL};

/// The bus's own name, destination and interface.
pub const DBUS_NAME: &str = "org.freedesktop.DBus";
pub const DBUS_PATH: &str = "/org/freedesktop/DBus";

// `RequestName` flags.
const NAME_FLAG_ALLOW_REPLACEMENT: u32 = 0x1;
const NAME_FLAG_REPLACE_EXISTING: u32 = 0x2;
const NAME_FLAG_DO_NOT_QUEUE: u32 = 0x4;

// `RequestName` replies.
const REQUEST_NAME_PRIMARY_OWNER: u32 = 1;
const REQUEST_NAME_IN_QUEUE: u32 = 2;
const REQUEST_NAME_EXISTS: u32 = 3;
const REQUEST_NAME_ALREADY_OWNER: u32 = 4;

// `ReleaseName` replies.
const RELEASE_NAME_RELEASED: u32 = 1;
const RELEASE_NAME_NON_EXISTENT: u32 = 2;
const RELEASE_NAME_NOT_OWNER: u32 = 3;

const ERR_INVALID_ARGS: &str = "org.freedesktop.DBus.Error.InvalidArgs";
const ERR_UNKNOWN_METHOD: &str = "org.freedesktop.DBus.Error.UnknownMethod";
const ERR_SERVICE_UNKNOWN: &str = "org.freedesktop.DBus.Error.ServiceUnknown";
const ERR_NAME_HAS_NO_OWNER: &str = "org.freedesktop.DBus.Error.NameHasNoOwner";
const ERR_ACCESS_DENIED: &str = "org.freedesktop.DBus.Error.AccessDenied";
const ERR_NOT_SUPPORTED: &str = "org.freedesktop.DBus.Error.NotSupported";

/// One client connection, as the bus sees it.
pub struct Conn {
    pub unique: String,
    /// `Hello` has been answered. Until then the only message the bus accepts
    /// is `Hello` itself (D-Bus specification, "Message Bus Starting Services").
    pub hello: bool,
    /// Well-known names this connection is the primary owner of.
    pub names: Vec<String>,
    pub rules: Vec<MatchRule>,
    pub pid: u32,
    pub uid: u32,
    /// `BecomeMonitor` was called: this connection receives a copy of
    /// everything and may no longer send. It is how `dbus-monitor` works, and
    /// on a machine with no debugger attached it is the only way to see what
    /// the session is actually saying.
    pub monitor: bool,
    /// Rules a monitor limited itself to; empty means everything.
    pub monitor_rules: Vec<MatchRule>,
}

/// Ownership of one well-known name: a primary owner plus the waiting queue.
struct NameEntry {
    owner: u64,
    owner_allows_replacement: bool,
    /// Waiting connections, oldest first: `(conn, allow_replacement)`.
    queue: Vec<(u64, bool)>,
}

pub struct Bus {
    /// The bus GUID, as sent in the `OK <guid>` auth reply and returned by
    /// `GetId`.
    pub guid: String,
    /// `/etc/machine-id`, returned by `org.freedesktop.DBus.Peer.GetMachineId`.
    pub machine_id: String,
    next_unique: u64,
    serial: u32,
    conns: BTreeMap<u64, Conn>,
    names: BTreeMap<String, NameEntry>,
    /// Messages the transport must write, in order.
    pub outbox: Vec<(u64, Message)>,
}

impl Bus {
    pub fn new(guid: String, machine_id: String) -> Self {
        Bus {
            guid,
            machine_id,
            next_unique: 1,
            serial: 1,
            conns: BTreeMap::new(),
            names: BTreeMap::new(),
            outbox: Vec::new(),
        }
    }

    #[allow(dead_code)]
    pub fn conn(&self, id: u64) -> Option<&Conn> {
        self.conns.get(&id)
    }

    /// Register a freshly authenticated connection. It gets its `:1.N` name
    /// here but is not announced until it says `Hello`.
    pub fn add_connection(&mut self, id: u64, pid: u32, uid: u32) {
        let unique = format!(":1.{}", self.next_unique);
        self.next_unique += 1;
        self.conns.insert(
            id,
            Conn {
                unique,
                hello: false,
                names: Vec::new(),
                rules: Vec::new(),
                pid,
                uid,
                monitor: false,
                monitor_rules: Vec::new(),
            },
        );
    }

    /// Drop a connection and release everything it owned, announcing each name
    /// change so clients waiting on `NameOwnerChanged` notice the exit.
    pub fn remove_connection(&mut self, id: u64) {
        let conn = match self.conns.remove(&id) {
            Some(c) => c,
            None => return,
        };
        // Names it owned outright: hand each to the next in the queue.
        for name in conn.names.clone() {
            self.transfer_name(&name, id, &conn.unique);
        }
        // Names it was merely queued for: just drop it from the queues.
        for entry in self.names.values_mut() {
            entry.queue.retain(|(c, _)| *c != id);
        }
        if conn.hello {
            self.broadcast_name_owner_changed(&conn.unique, &conn.unique, "");
        }
    }

    fn next_serial(&mut self) -> u32 {
        // Serial 0 is reserved.
        self.serial = self.serial.wrapping_add(1);
        if self.serial == 0 {
            self.serial = 1;
        }
        self.serial
    }

    fn send(&mut self, to: u64, mut msg: Message) {
        if msg.serial == 0 {
            msg.serial = self.next_serial();
        }
        self.outbox.push((to, msg));
    }

    /// Send a message that originates from the bus itself.
    fn send_from_bus(&mut self, to: u64, mut msg: Message) {
        msg.sender = Some(DBUS_NAME.to_string());
        if msg.serial == 0 {
            msg.serial = self.next_serial();
        }
        self.monitor_copy(None, &msg);
        self.send(to, msg);
    }

    /// Send a bus message, but place it at `at` in the outbox.
    ///
    /// `RequestName` and `ReleaseName` emit `NameAcquired`/`NameLost` and
    /// `NameOwnerChanged` as part of taking the name, yet the METHOD RETURN has
    /// to reach the caller first — that is the order dbus-daemon guarantees and
    /// libdbus's `dbus_bus_request_name()` blocks on the reply while the signals
    /// pile up behind it.
    fn send_from_bus_at(&mut self, at: usize, to: u64, mut msg: Message) {
        msg.sender = Some(DBUS_NAME.to_string());
        if msg.serial == 0 {
            msg.serial = self.next_serial();
        }
        let at = at.min(self.outbox.len());
        self.outbox.insert(at, (to, msg));
    }

    /// Give every monitor a copy of `msg`, which has already been stamped with
    /// its real sender. Called once per logical message, not once per
    /// recipient, so a broadcast shows up in `dbus-monitor` exactly once.
    fn monitor_copy(&mut self, from: Option<u64>, msg: &Message) {
        let sender_unique = msg.sender.clone().unwrap_or_default();
        let sender_names = from
            .and_then(|id| self.conns.get(&id))
            .map(|c| c.names.clone())
            .unwrap_or_default();
        let targets: Vec<u64> = self
            .conns
            .iter()
            .filter(|(cid, c)| {
                c.monitor
                    && Some(**cid) != from
                    && (c.monitor_rules.is_empty()
                        || c.monitor_rules
                            .iter()
                            .any(|r| r.matches(msg, &sender_unique, &sender_names)))
            })
            .map(|(cid, _)| *cid)
            .collect();
        for t in targets {
            self.outbox.push((t, msg.clone()));
        }
    }

    fn unique_of(&self, id: u64) -> String {
        self.conns
            .get(&id)
            .map(|c| c.unique.clone())
            .unwrap_or_default()
    }

    fn id_of_name(&self, name: &str) -> Option<u64> {
        if name == DBUS_NAME {
            return None; // handled by the bus, never routed to a connection
        }
        if let Some(rest) = name.strip_prefix(":1.") {
            let _ = rest;
            return self
                .conns
                .iter()
                .find(|(_, c)| c.unique == name)
                .map(|(id, _)| *id);
        }
        self.names.get(name).map(|e| e.owner)
    }

    // -----------------------------------------------------------------------
    // Routing
    // -----------------------------------------------------------------------

    /// Handle one message from connection `id`.
    pub fn dispatch(&mut self, id: u64, mut msg: Message) {
        let (unique, hello_done) = match self.conns.get(&id) {
            Some(c) => (c.unique.clone(), c.hello),
            None => return,
        };

        // Before Hello the connection has no identity, so nothing can be
        // routed from it and nothing can be routed to it.
        if !hello_done {
            let is_hello = msg.kind == MSG_METHOD_CALL
                && msg.member.as_deref() == Some("Hello")
                && msg.destination.as_deref() == Some(DBUS_NAME);
            if !is_hello {
                let e = Message::error(
                    &msg,
                    ERR_ACCESS_DENIED,
                    "Client tried to send a message other than Hello without being registered",
                );
                self.send_from_bus(id, e);
                return;
            }
        }

        // The bus stamps the sender on everything: a client cannot forge it.
        msg.sender = Some(unique.clone());
        self.monitor_copy(Some(id), &msg);

        match msg.destination.as_deref() {
            Some(DBUS_NAME) => self.handle_bus_call(id, &msg),
            Some(dest) => {
                let dest = dest.to_string();
                match self.id_of_name(&dest) {
                    Some(target) => {
                        // Watchers with a matching rule see it too; that is how
                        // `type='method_call'` rules are meant to work.
                        self.broadcast(id, &msg, Some(target));
                        self.send(target, msg);
                    }
                    None => {
                        if msg.kind == MSG_METHOD_CALL && msg.flags & FLAG_NO_REPLY_EXPECTED == 0 {
                            let e = Message::error(
                                &msg,
                                ERR_SERVICE_UNKNOWN,
                                &format!("The name {dest} was not provided by any .service files"),
                            );
                            self.send_from_bus(id, e);
                        }
                    }
                }
            }
            // No destination: a broadcast, which in practice is a signal.
            None => self.broadcast(id, &msg, None),
        }
    }

    /// Deliver `msg` to every connection with a matching rule, skipping the
    /// sender and `already` (the unicast destination, which got it directly).
    fn broadcast(&mut self, from: u64, msg: &Message, already: Option<u64>) {
        let (sender_unique, sender_names) = match self.conns.get(&from) {
            Some(c) => (c.unique.clone(), c.names.clone()),
            None => (String::new(), Vec::new()),
        };
        let targets: Vec<u64> = self
            .conns
            .iter()
            .filter(|(cid, c)| {
                **cid != from
                    && Some(**cid) != already
                    && c.hello
                    && c.rules
                        .iter()
                        .any(|r| r.matches(msg, &sender_unique, &sender_names))
            })
            .map(|(cid, _)| *cid)
            .collect();
        for t in targets {
            // The sender's own serial is preserved on a forward, exactly as
            // dbus-daemon does it: a client matches replies against the serial
            // IT chose, and a renumbered broadcast would break any client that
            // correlates a signal with the call that caused it.
            self.send(t, msg.clone());
        }
    }

    /// `NameOwnerChanged(name, old_owner, new_owner)` — the signal every
    /// client library watches to notice services coming and going.
    fn broadcast_name_owner_changed(&mut self, name: &str, old: &str, new: &str) {
        let mut sig = Message::signal(DBUS_PATH, DBUS_NAME, "NameOwnerChanged");
        sig.sender = Some(DBUS_NAME.to_string());
        sig.set_body(&[
            Arg::Str(name.to_string()),
            Arg::Str(old.to_string()),
            Arg::Str(new.to_string()),
        ]);
        sig.serial = self.next_serial();
        self.monitor_copy(None, &sig);
        let targets: Vec<u64> = self
            .conns
            .iter()
            .filter(|(_, c)| {
                c.hello
                    && c.rules
                        .iter()
                        .any(|r| r.matches(&sig, DBUS_NAME, &[DBUS_NAME.to_string()]))
            })
            .map(|(cid, _)| *cid)
            .collect();
        for t in targets {
            self.send(t, sig.clone());
        }
    }

    fn send_name_signal(&mut self, to: u64, member: &str, name: &str) {
        let mut sig = Message::signal(DBUS_PATH, DBUS_NAME, member);
        sig.destination = Some(self.unique_of(to));
        sig.set_body(&[Arg::Str(name.to_string())]);
        self.send_from_bus(to, sig);
    }

    // -----------------------------------------------------------------------
    // Name ownership
    // -----------------------------------------------------------------------

    /// Give `name` to the next queued connection after `from` gave it up.
    fn transfer_name(&mut self, name: &str, from: u64, from_unique: &str) {
        let next = match self.names.get_mut(name) {
            Some(entry) if entry.owner == from => {
                if entry.queue.is_empty() {
                    None
                } else {
                    let (next, allows) = entry.queue.remove(0);
                    entry.owner = next;
                    entry.owner_allows_replacement = allows;
                    Some(next)
                }
            }
            _ => return,
        };
        match next {
            Some(next) => {
                let next_unique = self.unique_of(next);
                if let Some(c) = self.conns.get_mut(&next) {
                    c.names.push(name.to_string());
                }
                self.send_name_signal(next, "NameAcquired", name);
                self.broadcast_name_owner_changed(name, from_unique, &next_unique);
            }
            None => {
                self.names.remove(name);
                self.broadcast_name_owner_changed(name, from_unique, "");
            }
        }
        if let Some(c) = self.conns.get_mut(&from) {
            c.names.retain(|n| n != name);
        }
    }

    fn request_name(&mut self, id: u64, name: &str, flags: u32) -> u32 {
        let unique = self.unique_of(id);
        let allow_replacement = flags & NAME_FLAG_ALLOW_REPLACEMENT != 0;

        let existing = match self.names.get(name) {
            None => {
                self.names.insert(
                    name.to_string(),
                    NameEntry {
                        owner: id,
                        owner_allows_replacement: allow_replacement,
                        queue: Vec::new(),
                    },
                );
                if let Some(c) = self.conns.get_mut(&id) {
                    c.names.push(name.to_string());
                }
                self.send_name_signal(id, "NameAcquired", name);
                self.broadcast_name_owner_changed(name, "", &unique);
                return REQUEST_NAME_PRIMARY_OWNER;
            }
            Some(e) => e,
        };

        if existing.owner == id {
            // Re-requesting only updates the replacement policy.
            if let Some(e) = self.names.get_mut(name) {
                e.owner_allows_replacement = allow_replacement;
            }
            return REQUEST_NAME_ALREADY_OWNER;
        }

        let can_replace =
            existing.owner_allows_replacement && flags & NAME_FLAG_REPLACE_EXISTING != 0;
        if can_replace {
            let old = existing.owner;
            let old_unique = self.unique_of(old);
            if let Some(c) = self.conns.get_mut(&old) {
                c.names.retain(|n| n != name);
            }
            if let Some(e) = self.names.get_mut(name) {
                // The displaced owner goes to the FRONT of the queue: it gets
                // the name back first when the new owner drops it.
                e.queue.insert(0, (old, e.owner_allows_replacement));
                e.owner = id;
                e.owner_allows_replacement = allow_replacement;
            }
            if let Some(c) = self.conns.get_mut(&id) {
                c.names.push(name.to_string());
            }
            self.send_name_signal(old, "NameLost", name);
            self.send_name_signal(id, "NameAcquired", name);
            self.broadcast_name_owner_changed(name, &old_unique, &unique);
            return REQUEST_NAME_PRIMARY_OWNER;
        }

        if flags & NAME_FLAG_DO_NOT_QUEUE != 0 {
            return REQUEST_NAME_EXISTS;
        }
        if let Some(e) = self.names.get_mut(name) {
            if !e.queue.iter().any(|(c, _)| *c == id) {
                e.queue.push((id, allow_replacement));
            }
        }
        REQUEST_NAME_IN_QUEUE
    }

    fn release_name(&mut self, id: u64, name: &str) -> u32 {
        let unique = self.unique_of(id);
        match self.names.get(name) {
            None => RELEASE_NAME_NON_EXISTENT,
            Some(e) if e.owner == id => {
                self.send_name_signal(id, "NameLost", name);
                self.transfer_name(name, id, &unique);
                RELEASE_NAME_RELEASED
            }
            Some(e) if e.queue.iter().any(|(c, _)| *c == id) => {
                if let Some(e) = self.names.get_mut(name) {
                    e.queue.retain(|(c, _)| *c != id);
                }
                RELEASE_NAME_RELEASED
            }
            Some(_) => RELEASE_NAME_NOT_OWNER,
        }
    }

    // -----------------------------------------------------------------------
    // org.freedesktop.DBus
    // -----------------------------------------------------------------------

    fn handle_bus_call(&mut self, id: u64, msg: &Message) {
        if msg.kind != MSG_METHOD_CALL {
            // Replies and signals addressed to the bus are simply dropped.
            return;
        }
        let member = msg.member.clone().unwrap_or_default();
        let iface = msg.interface.clone().unwrap_or_default();
        let args = msg.args();

        // Helpers keep each arm to one line; they take `&mut self` implicitly
        // through the closures' captures, so they are written out instead.
        macro_rules! reply {
            ($($a:expr),* $(,)?) => {{
                let mut r = Message::method_return(msg);
                r.set_body(&[$($a),*]);
                self.send_from_bus(id, r);
            }};
        }
        macro_rules! reply_empty {
            () => {{
                let r = Message::method_return(msg);
                self.send_from_bus(id, r);
            }};
        }
        macro_rules! fail {
            ($name:expr, $text:expr) => {{
                let e = Message::error(msg, $name, $text);
                self.send_from_bus(id, e);
                return;
            }};
        }

        let str_arg = |n: usize| -> Option<String> {
            match args.get(n) {
                Some(Arg::Str(s)) | Some(Arg::Path(s)) => Some(s.clone()),
                _ => None,
            }
        };
        let u32_arg = |n: usize| -> Option<u32> {
            match args.get(n) {
                Some(Arg::U32(v)) => Some(*v),
                _ => None,
            }
        };

        match (iface.as_str(), member.as_str()) {
            ("org.freedesktop.DBus.Peer", "Ping") => reply_empty!(),
            ("org.freedesktop.DBus.Peer", "GetMachineId") => {
                reply!(Arg::Str(self.machine_id.clone()))
            }
            ("org.freedesktop.DBus.Introspectable", "Introspect") => {
                reply!(Arg::Str(introspection_xml().to_string()))
            }
            ("org.freedesktop.DBus.Properties", "Get") => {
                let prop = str_arg(1).unwrap_or_default();
                match prop.as_str() {
                    "Features" => reply!(Arg::Variant(Box::new(Arg::StrArray(Vec::new())))),
                    "Interfaces" => reply!(Arg::Variant(Box::new(Arg::StrArray(Vec::new())))),
                    _ => fail!(
                        "org.freedesktop.DBus.Error.UnknownProperty",
                        "Unknown property"
                    ),
                }
            }
            ("org.freedesktop.DBus.Properties", "GetAll") => {
                // `a{sv}` is not in the writer's vocabulary and no client needs
                // the contents, so answer with the empty dictionary the
                // signature allows.
                let mut r = Message::method_return(msg);
                r.signature = Some("a{sv}".to_string());
                r.body = empty_dict_body(r.endian);
                self.send_from_bus(id, r);
            }
            ("org.freedesktop.DBus.Properties", "Set") => fail!(
                "org.freedesktop.DBus.Error.PropertyReadOnly",
                "Property is read-only"
            ),

            (_, "Hello") => {
                let unique = self.unique_of(id);
                if let Some(c) = self.conns.get_mut(&id) {
                    if c.hello {
                        fail!(ERR_ACCESS_DENIED, "Already handled an Hello message");
                    }
                    c.hello = true;
                }
                reply!(Arg::Str(unique.clone()));
                // Real dbus-daemon announces the unique name exactly like a
                // well-known one; clients such as GLib rely on the pair.
                self.send_name_signal(id, "NameAcquired", &unique);
                self.broadcast_name_owner_changed(&unique, "", &unique);
            }
            (_, "RequestName") => {
                let name = match str_arg(0) {
                    Some(n) => n,
                    None => fail!(ERR_INVALID_ARGS, "Expected a name and flags"),
                };
                if let Err(e) = validate_well_known_name(&name) {
                    fail!(ERR_INVALID_ARGS, &e);
                }
                let flags = u32_arg(1).unwrap_or(0);
                let mark = self.outbox.len();
                let code = self.request_name(id, &name, flags);
                let mut r = Message::method_return(msg);
                r.set_body(&[Arg::U32(code)]);
                self.send_from_bus_at(mark, id, r);
            }
            (_, "ReleaseName") => {
                let name = match str_arg(0) {
                    Some(n) => n,
                    None => fail!(ERR_INVALID_ARGS, "Expected a name"),
                };
                if let Err(e) = validate_well_known_name(&name) {
                    fail!(ERR_INVALID_ARGS, &e);
                }
                let mark = self.outbox.len();
                let code = self.release_name(id, &name);
                let mut r = Message::method_return(msg);
                r.set_body(&[Arg::U32(code)]);
                self.send_from_bus_at(mark, id, r);
            }
            (_, "ListNames") => {
                let mut names: Vec<String> = vec![DBUS_NAME.to_string()];
                names.extend(
                    self.conns
                        .values()
                        .filter(|c| c.hello)
                        .map(|c| c.unique.clone()),
                );
                names.extend(self.names.keys().cloned());
                reply!(Arg::StrArray(names));
            }
            (_, "ListActivatableNames") => {
                // Service activation is not implemented (see the module docs in
                // main.rs): only the bus itself is always available.
                reply!(Arg::StrArray(vec![DBUS_NAME.to_string()]));
            }
            (_, "NameHasOwner") => {
                let name = str_arg(0).unwrap_or_default();
                let has = name == DBUS_NAME || self.id_of_name(&name).is_some();
                reply!(Arg::Bool(has));
            }
            (_, "GetNameOwner") => {
                let name = str_arg(0).unwrap_or_default();
                if name == DBUS_NAME {
                    reply!(Arg::Str(DBUS_NAME.to_string()));
                } else {
                    match self.id_of_name(&name) {
                        Some(owner) => {
                            let u = self.unique_of(owner);
                            reply!(Arg::Str(u));
                        }
                        None => fail!(
                            ERR_NAME_HAS_NO_OWNER,
                            &format!("Could not get owner of name '{name}': no such name")
                        ),
                    }
                }
            }
            (_, "ListQueuedOwners") => {
                let name = str_arg(0).unwrap_or_default();
                match self.names.get(&name) {
                    Some(e) => {
                        let mut owners = vec![self.unique_of(e.owner)];
                        owners.extend(e.queue.iter().map(|(c, _)| self.unique_of(*c)));
                        reply!(Arg::StrArray(owners));
                    }
                    None => fail!(
                        ERR_NAME_HAS_NO_OWNER,
                        &format!("Could not get owners of name '{name}': no such name")
                    ),
                }
            }
            (_, "GetConnectionUnixUser") => {
                let name = str_arg(0).unwrap_or_default();
                match self.id_of_name(&name).and_then(|c| self.conns.get(&c)) {
                    Some(c) => {
                        let uid = c.uid;
                        reply!(Arg::U32(uid));
                    }
                    None => fail!(ERR_NAME_HAS_NO_OWNER, "No such name"),
                }
            }
            (_, "GetConnectionUnixProcessID") => {
                let name = str_arg(0).unwrap_or_default();
                match self.id_of_name(&name).and_then(|c| self.conns.get(&c)) {
                    Some(c) => {
                        let pid = c.pid;
                        reply!(Arg::U32(pid));
                    }
                    None => fail!(ERR_NAME_HAS_NO_OWNER, "No such name"),
                }
            }
            (_, "AddMatch") => {
                let rule = match str_arg(0) {
                    Some(r) => r,
                    None => fail!(ERR_INVALID_ARGS, "Expected a match rule string"),
                };
                if let Some(c) = self.conns.get_mut(&id) {
                    c.rules.push(MatchRule::parse(&rule));
                }
                reply_empty!();
            }
            (_, "RemoveMatch") => {
                let rule = match str_arg(0) {
                    Some(r) => r,
                    None => fail!(ERR_INVALID_ARGS, "Expected a match rule string"),
                };
                let removed = match self.conns.get_mut(&id) {
                    Some(c) => {
                        let before = c.rules.len();
                        // Remove one instance, like dbus-daemon does: rules are
                        // reference-counted per AddMatch call.
                        if let Some(pos) = c.rules.iter().position(|r| r.text == rule) {
                            c.rules.remove(pos);
                        }
                        c.rules.len() != before
                    }
                    None => false,
                };
                if removed {
                    reply_empty!();
                } else {
                    fail!(
                        "org.freedesktop.DBus.Error.MatchRuleNotFound",
                        "The given match rule was not found"
                    );
                }
            }
            (_, "GetId") => {
                reply!(Arg::Str(self.guid.clone()))
            }
            (_, "StartServiceByName") => {
                let name = str_arg(0).unwrap_or_default();
                if self.id_of_name(&name).is_some() {
                    // 2 == DBUS_START_REPLY_ALREADY_RUNNING
                    reply!(Arg::U32(2));
                } else {
                    fail!(
                        ERR_SERVICE_UNKNOWN,
                        &format!("The name {name} was not provided by any .service files")
                    );
                }
            }
            (_, "UpdateActivationEnvironment") | (_, "ReloadConfig") => reply_empty!(),
            (_, "GetConnectionCredentials") => {
                // `a{sv}` again: answer with the empty dictionary rather than
                // pretend to a marshaller this daemon does not have.
                let mut r = Message::method_return(msg);
                r.signature = Some("a{sv}".to_string());
                r.body = empty_dict_body(r.endian);
                self.send_from_bus(id, r);
            }
            (_, "GetAdtAuditSessionData") | (_, "GetConnectionSELinuxSecurityContext") => {
                fail!(ERR_NOT_SUPPORTED, "Not supported on Eclipse OS")
            }
            (_, "BecomeMonitor") => {
                let rules = match args.first() {
                    Some(Arg::StrArray(r)) => r.iter().map(|s| MatchRule::parse(s)).collect(),
                    _ => Vec::new(),
                };
                if let Some(c) = self.conns.get_mut(&id) {
                    c.monitor = true;
                    c.monitor_rules = rules;
                    // A monitor owns nothing and subscribes to nothing: it is
                    // off the routing table from here on.
                    c.rules.clear();
                }
                let names = self
                    .conns
                    .get(&id)
                    .map(|c| c.names.clone())
                    .unwrap_or_default();
                let unique = self.unique_of(id);
                for n in names {
                    self.transfer_name(&n, id, &unique);
                }
                reply_empty!();
            }
            _ => fail!(
                ERR_UNKNOWN_METHOD,
                &format!("Method '{member}' with signature on interface '{iface}' does not exist")
            ),
        }
    }
}

/// An empty `a{sv}`: the four-byte length word, zero.
fn empty_dict_body(endian: u8) -> Vec<u8> {
    let mut w = crate::message::Writer::new(endian);
    w.put_u32(0);
    w.into_bytes()
}

/// Well-known names must have at least two dot-separated elements, may not
/// start with a digit or a dot, and may only use `[A-Za-z0-9_-]`.
fn validate_well_known_name(name: &str) -> Result<(), String> {
    if name.starts_with(':') {
        return Err(format!("Cannot acquire a unique name ({name})"));
    }
    if name == DBUS_NAME {
        return Err("Cannot acquire the bus's own name".to_string());
    }
    if name.is_empty() || name.len() > 255 {
        return Err(format!("Invalid bus name '{name}'"));
    }
    let parts: Vec<&str> = name.split('.').collect();
    if parts.len() < 2 {
        return Err(format!("Invalid bus name '{name}': needs at least one dot"));
    }
    for p in parts {
        if p.is_empty() || p.starts_with(|c: char| c.is_ascii_digit()) {
            return Err(format!("Invalid bus name '{name}'"));
        }
        if !p
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(format!("Invalid bus name '{name}'"));
        }
    }
    Ok(())
}

fn introspection_xml() -> &'static str {
    concat!(
        "<!DOCTYPE node PUBLIC \"-//freedesktop//DTD D-BUS Object Introspection 1.0//EN\"\n",
        "\"http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd\">\n",
        "<node>\n",
        "  <interface name=\"org.freedesktop.DBus\">\n",
        "    <method name=\"Hello\"><arg direction=\"out\" type=\"s\"/></method>\n",
        "    <method name=\"RequestName\"><arg direction=\"in\" type=\"s\"/><arg direction=\"in\" type=\"u\"/><arg direction=\"out\" type=\"u\"/></method>\n",
        "    <method name=\"ReleaseName\"><arg direction=\"in\" type=\"s\"/><arg direction=\"out\" type=\"u\"/></method>\n",
        "    <method name=\"ListNames\"><arg direction=\"out\" type=\"as\"/></method>\n",
        "    <method name=\"ListActivatableNames\"><arg direction=\"out\" type=\"as\"/></method>\n",
        "    <method name=\"NameHasOwner\"><arg direction=\"in\" type=\"s\"/><arg direction=\"out\" type=\"b\"/></method>\n",
        "    <method name=\"GetNameOwner\"><arg direction=\"in\" type=\"s\"/><arg direction=\"out\" type=\"s\"/></method>\n",
        "    <method name=\"ListQueuedOwners\"><arg direction=\"in\" type=\"s\"/><arg direction=\"out\" type=\"as\"/></method>\n",
        "    <method name=\"GetConnectionUnixUser\"><arg direction=\"in\" type=\"s\"/><arg direction=\"out\" type=\"u\"/></method>\n",
        "    <method name=\"GetConnectionUnixProcessID\"><arg direction=\"in\" type=\"s\"/><arg direction=\"out\" type=\"u\"/></method>\n",
        "    <method name=\"AddMatch\"><arg direction=\"in\" type=\"s\"/></method>\n",
        "    <method name=\"RemoveMatch\"><arg direction=\"in\" type=\"s\"/></method>\n",
        "    <method name=\"GetId\"><arg direction=\"out\" type=\"s\"/></method>\n",
        "    <method name=\"StartServiceByName\"><arg direction=\"in\" type=\"s\"/><arg direction=\"in\" type=\"u\"/><arg direction=\"out\" type=\"u\"/></method>\n",
        "    <signal name=\"NameOwnerChanged\"><arg type=\"s\"/><arg type=\"s\"/><arg type=\"s\"/></signal>\n",
        "    <signal name=\"NameLost\"><arg type=\"s\"/></signal>\n",
        "    <signal name=\"NameAcquired\"><arg type=\"s\"/></signal>\n",
        "  </interface>\n",
        "  <interface name=\"org.freedesktop.DBus.Peer\">\n",
        "    <method name=\"Ping\"/>\n",
        "    <method name=\"GetMachineId\"><arg direction=\"out\" type=\"s\"/></method>\n",
        "  </interface>\n",
        "  <interface name=\"org.freedesktop.DBus.Introspectable\">\n",
        "    <method name=\"Introspect\"><arg direction=\"out\" type=\"s\"/></method>\n",
        "  </interface>\n",
        "</node>\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Arg, MSG_ERROR, MSG_METHOD_RETURN};

    fn bus() -> Bus {
        Bus::new("abc".into(), "0123456789abcdef0123456789abcdef".into())
    }

    /// Connect `id` and complete the `Hello` handshake, returning its `:1.N`.
    /// Only the handshake traffic addressed to `id` is dropped: a watcher's
    /// `NameOwnerChanged` is exactly what some tests are looking for.
    fn hello(b: &mut Bus, id: u64) -> String {
        b.add_connection(id, 100 + id as u32, 0);
        let mut m = Message::method_call(DBUS_NAME, DBUS_PATH, DBUS_NAME, "Hello");
        m.serial = 1;
        b.dispatch(id, m);
        let unique = b.conn(id).unwrap().unique.clone();
        b.outbox.retain(|(to, _)| *to != id);
        unique
    }

    fn take(b: &mut Bus) -> Vec<(u64, Message)> {
        std::mem::take(&mut b.outbox)
    }

    #[test]
    fn hello_assigns_unique_name_and_announces_it() {
        let mut b = bus();
        b.add_connection(1, 42, 0);
        let mut m = Message::method_call(DBUS_NAME, DBUS_PATH, DBUS_NAME, "Hello");
        m.serial = 1;
        b.dispatch(1, m);
        let out = take(&mut b);
        assert_eq!(out[0].1.kind, MSG_METHOD_RETURN);
        assert_eq!(out[0].1.args(), vec![Arg::Str(":1.1".into())]);
        assert_eq!(out[0].1.sender.as_deref(), Some(DBUS_NAME));
        assert_eq!(out[1].1.member.as_deref(), Some("NameAcquired"));
        assert_eq!(out[1].1.args(), vec![Arg::Str(":1.1".into())]);
    }

    #[test]
    fn message_before_hello_is_refused() {
        let mut b = bus();
        b.add_connection(1, 42, 0);
        let mut m = Message::method_call(DBUS_NAME, DBUS_PATH, DBUS_NAME, "ListNames");
        m.serial = 1;
        b.dispatch(1, m);
        let out = take(&mut b);
        assert_eq!(out[0].1.kind, MSG_ERROR);
        assert_eq!(out[0].1.error_name.as_deref(), Some(ERR_ACCESS_DENIED));
    }

    #[test]
    fn request_name_then_route_by_well_known_name() {
        let mut b = bus();
        let _a = hello(&mut b, 1);
        let bb = hello(&mut b, 2);

        let mut req = Message::method_call(DBUS_NAME, DBUS_PATH, DBUS_NAME, "RequestName");
        req.serial = 2;
        req.set_body(&[Arg::Str("org.example.Svc".into()), Arg::U32(0)]);
        b.dispatch(2, req);
        let out = take(&mut b);
        assert_eq!(out[0].1.args(), vec![Arg::U32(REQUEST_NAME_PRIMARY_OWNER)]);

        // Now a call from conn 1 to the well-known name lands on conn 2, with
        // the sender stamped by the bus.
        let mut call = Message::method_call("org.example.Svc", "/", "org.example", "Do");
        call.serial = 3;
        b.dispatch(1, call);
        let out = take(&mut b);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, 2);
        assert_eq!(out[0].1.sender.as_deref(), Some(":1.1"));
        assert_eq!(out[0].1.destination.as_deref(), Some("org.example.Svc"));
        let _ = bb;
    }

    #[test]
    fn call_to_unknown_name_gets_service_unknown() {
        let mut b = bus();
        hello(&mut b, 1);
        let mut call = Message::method_call("org.example.Nope", "/", "org.example", "Do");
        call.serial = 2;
        b.dispatch(1, call);
        let out = take(&mut b);
        assert_eq!(out[0].1.kind, MSG_ERROR);
        assert_eq!(out[0].1.error_name.as_deref(), Some(ERR_SERVICE_UNKNOWN));
    }

    #[test]
    fn no_reply_expected_call_to_unknown_name_is_silent() {
        let mut b = bus();
        hello(&mut b, 1);
        let mut call = Message::method_call("org.example.Nope", "/", "org.example", "Do");
        call.serial = 2;
        call.flags = FLAG_NO_REPLY_EXPECTED;
        b.dispatch(1, call);
        assert!(take(&mut b).is_empty());
    }

    #[test]
    fn signals_go_only_to_matching_subscribers() {
        let mut b = bus();
        hello(&mut b, 1); // emitter
        hello(&mut b, 2); // subscriber
        hello(&mut b, 3); // uninterested

        let mut add = Message::method_call(DBUS_NAME, DBUS_PATH, DBUS_NAME, "AddMatch");
        add.serial = 2;
        add.set_body(&[Arg::Str("type='signal',interface='org.example'".into())]);
        b.dispatch(2, add);
        take(&mut b);

        let mut sig = Message::signal("/x", "org.example", "Boom");
        sig.serial = 3;
        b.dispatch(1, sig);
        let out = take(&mut b);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, 2);
        assert_eq!(out[0].1.member.as_deref(), Some("Boom"));
        assert_eq!(out[0].1.sender.as_deref(), Some(":1.1"));
        assert_ne!(out[0].1.serial, 0, "every forwarded copy gets a serial");
    }

    #[test]
    fn name_owner_changed_reaches_watchers() {
        let mut b = bus();
        hello(&mut b, 1);
        let mut add = Message::method_call(DBUS_NAME, DBUS_PATH, DBUS_NAME, "AddMatch");
        add.serial = 2;
        add.set_body(&[Arg::Str(
            "type='signal',interface='org.freedesktop.DBus',member='NameOwnerChanged'".into(),
        )]);
        b.dispatch(1, add);
        take(&mut b);

        hello(&mut b, 2);
        let out = take(&mut b);
        // conn 1 watches, so it saw :1.2 appear.
        let seen: Vec<_> = out
            .iter()
            .filter(|(to, m)| *to == 1 && m.member.as_deref() == Some("NameOwnerChanged"))
            .collect();
        assert_eq!(seen.len(), 1);
        assert_eq!(
            seen[0].1.args(),
            vec![
                Arg::Str(":1.2".into()),
                Arg::Str(String::new()),
                Arg::Str(":1.2".into())
            ]
        );
    }

    #[test]
    fn disconnect_releases_name_and_announces_it() {
        let mut b = bus();
        hello(&mut b, 1);
        hello(&mut b, 2);
        let mut add = Message::method_call(DBUS_NAME, DBUS_PATH, DBUS_NAME, "AddMatch");
        add.serial = 2;
        add.set_body(&[Arg::Str("type='signal',member='NameOwnerChanged'".into())]);
        b.dispatch(1, add);
        let mut req = Message::method_call(DBUS_NAME, DBUS_PATH, DBUS_NAME, "RequestName");
        req.serial = 3;
        req.set_body(&[Arg::Str("org.example.Svc".into()), Arg::U32(0)]);
        b.dispatch(2, req);
        take(&mut b);

        b.remove_connection(2);
        let out = take(&mut b);
        let names: Vec<Vec<Arg>> = out
            .iter()
            .filter(|(to, m)| *to == 1 && m.member.as_deref() == Some("NameOwnerChanged"))
            .map(|(_, m)| m.args())
            .collect();
        assert!(names.contains(&vec![
            Arg::Str("org.example.Svc".into()),
            Arg::Str(":1.2".into()),
            Arg::Str(String::new())
        ]));
        assert!(names.contains(&vec![
            Arg::Str(":1.2".into()),
            Arg::Str(":1.2".into()),
            Arg::Str(String::new())
        ]));
    }

    #[test]
    fn queued_owner_takes_over_on_release() {
        let mut b = bus();
        hello(&mut b, 1);
        hello(&mut b, 2);
        for id in [1u64, 2u64] {
            let mut req = Message::method_call(DBUS_NAME, DBUS_PATH, DBUS_NAME, "RequestName");
            req.serial = 2;
            req.set_body(&[Arg::Str("org.example.Svc".into()), Arg::U32(0)]);
            b.dispatch(id, req);
        }
        let out = take(&mut b);
        let codes: Vec<Vec<Arg>> = out
            .iter()
            .filter(|(_, m)| m.kind == MSG_METHOD_RETURN)
            .map(|(_, m)| m.args())
            .collect();
        assert_eq!(codes[0], vec![Arg::U32(REQUEST_NAME_PRIMARY_OWNER)]);
        assert_eq!(codes[1], vec![Arg::U32(REQUEST_NAME_IN_QUEUE)]);

        let mut rel = Message::method_call(DBUS_NAME, DBUS_PATH, DBUS_NAME, "ReleaseName");
        rel.serial = 3;
        rel.set_body(&[Arg::Str("org.example.Svc".into())]);
        b.dispatch(1, rel);
        take(&mut b);

        // Conn 2 now owns it: a call routes there.
        let mut call = Message::method_call("org.example.Svc", "/", "org.example", "Do");
        call.serial = 4;
        b.dispatch(1, call);
        let out = take(&mut b);
        assert_eq!(out[0].0, 2);
    }

    #[test]
    fn do_not_queue_reports_exists() {
        let mut b = bus();
        hello(&mut b, 1);
        hello(&mut b, 2);
        for id in [1u64, 2u64] {
            let mut req = Message::method_call(DBUS_NAME, DBUS_PATH, DBUS_NAME, "RequestName");
            req.serial = 2;
            req.set_body(&[
                Arg::Str("org.example.Svc".into()),
                Arg::U32(NAME_FLAG_DO_NOT_QUEUE),
            ]);
            b.dispatch(id, req);
        }
        let out = take(&mut b);
        let codes: Vec<Vec<Arg>> = out
            .iter()
            .filter(|(_, m)| m.kind == MSG_METHOD_RETURN)
            .map(|(_, m)| m.args())
            .collect();
        assert_eq!(codes[1], vec![Arg::U32(REQUEST_NAME_EXISTS)]);
    }

    #[test]
    fn replace_existing_needs_allow_replacement() {
        let mut b = bus();
        hello(&mut b, 1);
        hello(&mut b, 2);
        let mut req = Message::method_call(DBUS_NAME, DBUS_PATH, DBUS_NAME, "RequestName");
        req.serial = 2;
        req.set_body(&[
            Arg::Str("org.example.Svc".into()),
            Arg::U32(NAME_FLAG_ALLOW_REPLACEMENT),
        ]);
        b.dispatch(1, req);
        take(&mut b);

        let mut req2 = Message::method_call(DBUS_NAME, DBUS_PATH, DBUS_NAME, "RequestName");
        req2.serial = 3;
        req2.set_body(&[
            Arg::Str("org.example.Svc".into()),
            Arg::U32(NAME_FLAG_REPLACE_EXISTING),
        ]);
        b.dispatch(2, req2);
        let out = take(&mut b);
        let ret = out
            .iter()
            .find(|(_, m)| m.kind == MSG_METHOD_RETURN)
            .unwrap();
        assert_eq!(ret.1.args(), vec![Arg::U32(REQUEST_NAME_PRIMARY_OWNER)]);
        assert!(out
            .iter()
            .any(|(to, m)| *to == 1 && m.member.as_deref() == Some("NameLost")));
    }

    #[test]
    fn invalid_name_is_rejected() {
        let mut b = bus();
        hello(&mut b, 1);
        for bad in ["nodot", ":1.5", "org.freedesktop.DBus", "org..x", "org.9x"] {
            let mut req = Message::method_call(DBUS_NAME, DBUS_PATH, DBUS_NAME, "RequestName");
            req.serial = 2;
            req.set_body(&[Arg::Str(bad.into()), Arg::U32(0)]);
            b.dispatch(1, req);
            let out = take(&mut b);
            assert_eq!(out[0].1.kind, MSG_ERROR, "{bad} should be rejected");
        }
    }

    #[test]
    fn list_names_includes_bus_unique_and_well_known() {
        let mut b = bus();
        hello(&mut b, 1);
        let mut req = Message::method_call(DBUS_NAME, DBUS_PATH, DBUS_NAME, "RequestName");
        req.serial = 2;
        req.set_body(&[Arg::Str("org.example.Svc".into()), Arg::U32(0)]);
        b.dispatch(1, req);
        take(&mut b);

        let mut m = Message::method_call(DBUS_NAME, DBUS_PATH, DBUS_NAME, "ListNames");
        m.serial = 3;
        b.dispatch(1, m);
        let out = take(&mut b);
        match &out[0].1.args()[0] {
            Arg::StrArray(names) => {
                assert!(names.contains(&DBUS_NAME.to_string()));
                assert!(names.contains(&":1.1".to_string()));
                assert!(names.contains(&"org.example.Svc".to_string()));
            }
            other => panic!("unexpected reply {other:?}"),
        }
    }

    #[test]
    fn unknown_bus_method_is_an_error_not_a_hang() {
        let mut b = bus();
        hello(&mut b, 1);
        let mut m = Message::method_call(DBUS_NAME, DBUS_PATH, DBUS_NAME, "NoSuchMethod");
        m.serial = 2;
        b.dispatch(1, m);
        let out = take(&mut b);
        assert_eq!(out[0].1.kind, MSG_ERROR);
        assert_eq!(out[0].1.error_name.as_deref(), Some(ERR_UNKNOWN_METHOD));
    }

    #[test]
    fn ping_and_get_machine_id() {
        let mut b = bus();
        hello(&mut b, 1);
        let mut p = Message::method_call(DBUS_NAME, DBUS_PATH, "org.freedesktop.DBus.Peer", "Ping");
        p.serial = 2;
        b.dispatch(1, p);
        let out = take(&mut b);
        assert_eq!(out[0].1.kind, MSG_METHOD_RETURN);
        assert!(out[0].1.signature.is_none());

        let mut g = Message::method_call(
            DBUS_NAME,
            DBUS_PATH,
            "org.freedesktop.DBus.Peer",
            "GetMachineId",
        );
        g.serial = 3;
        b.dispatch(1, g);
        let out = take(&mut b);
        assert_eq!(
            out[0].1.args(),
            vec![Arg::Str("0123456789abcdef0123456789abcdef".into())]
        );
    }

    #[test]
    fn remove_match_reports_unknown_rule() {
        let mut b = bus();
        hello(&mut b, 1);
        let mut m = Message::method_call(DBUS_NAME, DBUS_PATH, DBUS_NAME, "RemoveMatch");
        m.serial = 2;
        m.set_body(&[Arg::Str("type='signal'".into())]);
        b.dispatch(1, m);
        let out = take(&mut b);
        assert_eq!(out[0].1.kind, MSG_ERROR);
    }

    #[test]
    fn reply_routes_back_to_the_caller() {
        let mut b = bus();
        let a = hello(&mut b, 1);
        hello(&mut b, 2);
        let mut reply = Message {
            endian: crate::message::native_endian(),
            kind: MSG_METHOD_RETURN,
            serial: 5,
            reply_serial: Some(3),
            destination: Some(a.clone()),
            ..Default::default()
        };
        reply.set_body(&[Arg::U32(1)]);
        b.dispatch(2, reply);
        let out = take(&mut b);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, 1);
        assert_eq!(out[0].1.sender.as_deref(), Some(":1.2"));
    }
}
