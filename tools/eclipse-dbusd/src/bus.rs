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
        self.monitor_copy(None, Some(to), &msg);
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
    ///
    /// "Exactly once" needs BOTH exclusions and used to have only one. `from`
    /// keeps a monitor from seeing its own traffic echoed; `to` keeps it from
    /// seeing a message twice when it is itself the recipient -- once as the
    /// monitor copy and once as the addressee. The bus answers `BecomeMonitor`
    /// through `send_from_bus`, which copies to every monitor and then sends to
    /// the caller, so the very call that turns a connection into a monitor got
    /// its own METHOD_RETURN twice, and so did every later reply and error the
    /// bus sent it.
    fn monitor_copy(&mut self, from: Option<u64>, to: Option<u64>, msg: &Message) {
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
                    // Redundant while `dispatch` refuses a monitor's messages
                    // outright -- `from` is never a monitor today -- and kept
                    // because it is the exclusion that stops the echo if that
                    // guard is ever relaxed. It costs one comparison.
                    && Some(**cid) != from
                    && Some(**cid) != to
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

    /// The connection a name belongs to, or `None` for a name no connection
    /// holds.
    ///
    /// Both of the first two lines are there to say what is meant rather than
    /// to change the answer. No connection can hold the bus's own name
    /// (`validate_well_known_name` refuses it) and none can hold anything
    /// starting with a colon (same), so the fall-through would answer `None`
    /// for the first and the full prefix is only as precise as `":"` would be.
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
        let (unique, hello_done, is_monitor) = match self.conns.get(&id) {
            Some(c) => (c.unique.clone(), c.hello, c.monitor),
            None => return,
        };

        // A monitor receives everything and sends nothing. `Conn::monitor` has
        // said so since it was written and nothing enforced it, so a monitor's
        // traffic was routed like anybody else's -- and a monitor sees its own
        // messages come back through `monitor_copy`, which is a feedback loop no
        // ordinary client can create. dbus-daemon disconnects a monitor that
        // sends; `Bus` owns no socket, so the nearest thing it can do is refuse
        // the message and say why.
        if is_monitor {
            let e = Message::error(
                &msg,
                ERR_ACCESS_DENIED,
                "A monitor connection may not send messages",
            );
            self.send_from_bus(id, e);
            return;
        }

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
        // The recipient is not known yet (it depends on the destination), so a
        // monitor addressed by its own unique name would get two copies. It
        // cannot be addressed: a monitor is refused above before it can tell
        // anyone its name, and `BecomeMonitor` gives up the names it had.
        self.monitor_copy(Some(id), None, &msg);

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
                    // Redundant today and kept for what it says: a connection
                    // that has not said Hello cannot have called AddMatch, so
                    // its rule list is empty and the test below fails anyway.
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
        self.monitor_copy(None, None, &sig);
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
                        if let Some(pos) = find_rule(&c.rules, &rule) {
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

/// Which of `rules` is the one `text` names, for `RemoveMatch`.
///
/// An exact text match wins, because rules are reference-counted per `AddMatch`
/// call and a client that adds the same string twice must remove it twice.
/// Failing that, a rule that PARSES the same counts: the comparison used to be
/// `r.text == rule` alone, so `"type='signal', member='X'"` could not remove
/// `"type='signal',member='X'"` -- one space -- and the rule stayed for the life
/// of the connection. Nothing ever reports it: the client believes it
/// unsubscribed and keeps receiving the traffic, and every message the bus
/// routes is tested against the rule again for as long as the client lives.
fn find_rule(rules: &[MatchRule], text: &str) -> Option<usize> {
    // The fast path picks a different INDEX from the fallback when two
    // equivalent rules are spelt differently, and never a different rule: both
    // select the same traffic, so removing either is the same removal. It is
    // here to keep the common case off the parser, not for the answer.
    if let Some(pos) = rules.iter().position(|r| r.text == text) {
        return Some(pos);
    }
    let want = MatchRule::parse(text);
    rules.iter().position(|r| same_rule(r, &want))
}

/// Do two rules select the same traffic? Everything but the original text.
fn same_rule(a: &MatchRule, b: &MatchRule) -> bool {
    a.kind == b.kind
        && a.sender == b.sender
        && a.interface == b.interface
        && a.member == b.member
        && a.path == b.path
        && a.path_namespace == b.path_namespace
        && a.destination == b.destination
        && a.arg0 == b.arg0
        && a.arg0namespace == b.arg0namespace
        && a.unsupported == b.unsupported
}

/// An empty `a{sv}`: the four-byte length word, zero, and then the padding to
/// the element's own alignment.
///
/// The padding is NOT optional and used to be missing. The specification reads
/// "a UINT32 giving the length of the array data in bytes, followed by alignment
/// padding to the alignment boundary of the array element type, followed by each
/// array element" -- unconditionally, so an empty `a{sv}` is eight bytes and not
/// four, because `DICT_ENTRY` aligns to 8. The crate's own `Arg::DictSS` writer
/// has always emitted it (`w.align(8)` before the first entry); this
/// hand-rolled twin did not, so the two disagreed about the same array and the
/// bodies of `GetConnectionCredentials` and `GetAll` were four bytes short of
/// where a reader that follows the spec stops. Tolerated by the readers we could
/// try, which is exactly why it survived.
fn empty_dict_body(endian: u8) -> Vec<u8> {
    let mut w = crate::message::Writer::new(endian);
    w.put_u32(0);
    w.align(8); // DICT_ENTRY
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
    // The empty name is refused by the dot rule below as well -- `"".split('.')`
    // is one empty element -- so this half only changes the wording of the
    // error. The length is the half that matters.
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
    /// Call one of the bus's own methods with a single string argument.
    fn bus_call(b: &mut Bus, id: u64, serial: u32, member: &str, arg: &str) {
        let mut m = Message::method_call(DBUS_NAME, DBUS_PATH, DBUS_NAME, member);
        m.serial = serial;
        m.set_body(&[Arg::Str(arg.to_string())]);
        b.dispatch(id, m);
    }

    /// A signal from `id` that `type='signal',interface='org.example'` matches.
    fn example_signal(b: &mut Bus, id: u64, serial: u32) {
        let mut sig = Message::signal("/org/a", "org.example", "Ping");
        sig.serial = serial;
        b.dispatch(id, sig);
    }

    #[test]
    fn a_monitor_may_not_send() {
        // `Conn::monitor` has said "may no longer send" since it was written and
        // nothing enforced it, so a monitor's own traffic was routed like
        // anybody else's -- and a monitor gets a copy of every message, its own
        // included, which is a loop no ordinary client can make. dbus-daemon
        // disconnects a monitor that sends.
        let mut b = bus();
        let one = hello(&mut b, 1);
        hello(&mut b, 2);
        let mut m = Message::method_call(DBUS_NAME, DBUS_PATH, DBUS_NAME, "BecomeMonitor");
        m.serial = 2;
        b.dispatch(2, m);
        take(&mut b);

        // A method call from the monitor to a real connection is refused, and
        // connection 1 hears nothing.
        let mut call = Message::method_call(&one, "/org/a", "org.example", "M");
        call.serial = 3;
        b.dispatch(2, call);
        let out = take(&mut b);
        assert!(
            out.iter().all(|(to, _)| *to == 2),
            "algo del monitor llego a otro: {out:?}"
        );
        assert_eq!(out.len(), 1, "una sola respuesta, el error: {out:?}");
        assert_eq!(out[0].1.kind, MSG_ERROR);
        assert_eq!(
            out[0].1.error_name.as_deref(),
            Some("org.freedesktop.DBus.Error.AccessDenied")
        );
        assert_eq!(out[0].1.reply_serial, Some(3));

        // Not even a signal, which has no destination and would otherwise reach
        // every subscriber.
        example_signal(&mut b, 2, 4);
        let out = take(&mut b);
        assert!(
            out.iter().all(|(to, _)| *to == 2),
            "una senal del monitor se difundio: {out:?}"
        );

        // And an ordinary connection still works, so the guard is on the monitor
        // and not on everybody.
        example_signal(&mut b, 1, 5);
        let out = take(&mut b);
        assert!(
            out.iter().any(|(to, _)| *to == 2),
            "el monitor dejo de recibir: {out:?}"
        );
    }

    #[test]
    fn a_monitor_sees_every_message_exactly_once() {
        // The promise on `monitor_copy` is "exactly once", and it used to hold
        // only for messages the monitor was not itself the recipient of: the
        // copy went out to every monitor and THEN the message went to its
        // addressee, so a monitor got two of everything the bus sent it -- the
        // reply to its own `BecomeMonitor` first of all. In dbus-monitor that
        // reads as the session saying everything twice.
        let mut b = bus();
        hello(&mut b, 1);
        hello(&mut b, 2);
        let mut m = Message::method_call(DBUS_NAME, DBUS_PATH, DBUS_NAME, "BecomeMonitor");
        m.serial = 2;
        b.dispatch(2, m);
        let out = take(&mut b);
        let mine: Vec<_> = out.iter().filter(|(to, _)| *to == 2).collect();
        assert_eq!(
            mine.len(),
            1,
            "la respuesta a BecomeMonitor, duplicada: {mine:?}"
        );
        assert_eq!(mine[0].1.kind, MSG_METHOD_RETURN);

        // A broadcast from somebody else: one copy, and only the monitor copy.
        example_signal(&mut b, 1, 3);
        let out = take(&mut b);
        let mine: Vec<_> = out.iter().filter(|(to, _)| *to == 2).collect();
        assert_eq!(mine.len(), 1, "la senal, duplicada: {mine:?}");

        // And an error the bus sends the monitor: one copy.
        bus_call(&mut b, 1, 4, "GetNameOwner", "org.no.Such.Name");
        take(&mut b);
        let mut bad = Message::method_call(DBUS_NAME, DBUS_PATH, DBUS_NAME, "NoSuchMethod");
        bad.serial = 5;
        b.dispatch(2, bad);
        let out = take(&mut b);
        let mine: Vec<_> = out.iter().filter(|(to, _)| *to == 2).collect();
        assert_eq!(mine.len(), 1, "el error, duplicado: {mine:?}");
    }

    #[test]
    fn a_rule_removed_with_different_spacing_is_actually_removed() {
        // `RemoveMatch` compared the rule TEXT, so one space of difference left
        // the rule in place for the life of the connection: the client believes
        // it unsubscribed, keeps receiving the traffic, and every message the bus
        // routes is tested against the rule again. libdbus compares the parse.
        let mut b = bus();
        hello(&mut b, 1);
        hello(&mut b, 2);
        bus_call(
            &mut b,
            2,
            2,
            "AddMatch",
            "type='signal', interface='org.example'",
        );
        take(&mut b);
        // It is subscribed.
        example_signal(&mut b, 1, 3);
        assert!(
            take(&mut b).iter().any(|(to, _)| *to == 2),
            "la regla no estaba puesta"
        );

        bus_call(
            &mut b,
            2,
            4,
            "RemoveMatch",
            "type='signal',interface='org.example'",
        );
        let out = take(&mut b);
        assert_eq!(
            out[0].1.kind, MSG_METHOD_RETURN,
            "RemoveMatch fallo: {:?}",
            out[0].1.error_name
        );
        assert_eq!(
            b.conn(2).unwrap().rules.len(),
            0,
            "la regla se quedo puesta"
        );
        // And the traffic really stops.
        example_signal(&mut b, 1, 5);
        assert!(
            take(&mut b).iter().all(|(to, _)| *to != 2),
            "sigue recibiendo despues de quitar la regla"
        );
    }

    #[test]
    fn the_same_rule_added_twice_has_to_be_removed_twice() {
        // Rules are reference-counted per AddMatch call, which is what two
        // libraries inside one process depend on: GIO and libdbus both subscribe
        // to NameOwnerChanged, and the first one to unsubscribe must not take
        // the other's subscription with it.
        let mut b = bus();
        hello(&mut b, 1);
        hello(&mut b, 2);
        let rule = "type='signal',interface='org.example'";
        bus_call(&mut b, 2, 2, "AddMatch", rule);
        bus_call(&mut b, 2, 3, "AddMatch", rule);
        take(&mut b);
        assert_eq!(b.conn(2).unwrap().rules.len(), 2);

        bus_call(&mut b, 2, 4, "RemoveMatch", rule);
        take(&mut b);
        assert_eq!(b.conn(2).unwrap().rules.len(), 1, "se quitaron las dos");
        example_signal(&mut b, 1, 5);
        assert!(
            take(&mut b).iter().any(|(to, _)| *to == 2),
            "una sola llamada a RemoveMatch cancelo las dos suscripciones"
        );

        bus_call(&mut b, 2, 6, "RemoveMatch", rule);
        take(&mut b);
        assert_eq!(b.conn(2).unwrap().rules.len(), 0);
        // A third try is an error, not a silent success.
        bus_call(&mut b, 2, 7, "RemoveMatch", rule);
        let out = take(&mut b);
        assert_eq!(out[0].1.kind, MSG_ERROR);
        assert_eq!(
            out[0].1.error_name.as_deref(),
            Some("org.freedesktop.DBus.Error.MatchRuleNotFound")
        );
    }

    #[test]
    fn a_rule_that_is_not_the_same_rule_is_not_removed() {
        // The fallback compares what the rule SELECTS, so it must not collapse
        // two rules that select different traffic -- otherwise RemoveMatch
        // silently cancels somebody else's subscription.
        let mut b = bus();
        hello(&mut b, 1);
        bus_call(&mut b, 1, 2, "AddMatch", "type='signal',member='A'");
        take(&mut b);
        bus_call(&mut b, 1, 3, "RemoveMatch", "type='signal',member='B'");
        let out = take(&mut b);
        assert_eq!(out[0].1.kind, MSG_ERROR, "quito una regla distinta");
        assert_eq!(b.conn(1).unwrap().rules.len(), 1);
        // Reordered keys ARE the same rule, which is the case the text
        // comparison could not see either.
        bus_call(&mut b, 1, 4, "RemoveMatch", "member='A',type='signal'");
        let out = take(&mut b);
        assert_eq!(out[0].1.kind, MSG_METHOD_RETURN, "el orden de las claves");
        assert_eq!(b.conn(1).unwrap().rules.len(), 0);

        // Every field has to count, one at a time: a comparison that dropped
        // one would let a RemoveMatch cancel a subscription the client never
        // asked to cancel, and nothing reports that either.
        let base = "type='signal',sender=':1.9',interface='org.e',member='M',                    path='/p',path_namespace='/n',destination=':1.8',arg0='a',                    arg0namespace='n'";
        for other in [
            "type='method_call',sender=':1.9',interface='org.e',member='M',path='/p',path_namespace='/n',destination=':1.8',arg0='a',arg0namespace='n'",
            "type='signal',sender=':1.7',interface='org.e',member='M',path='/p',path_namespace='/n',destination=':1.8',arg0='a',arg0namespace='n'",
            "type='signal',sender=':1.9',interface='org.OTRA',member='M',path='/p',path_namespace='/n',destination=':1.8',arg0='a',arg0namespace='n'",
            "type='signal',sender=':1.9',interface='org.e',member='OTRO',path='/p',path_namespace='/n',destination=':1.8',arg0='a',arg0namespace='n'",
            "type='signal',sender=':1.9',interface='org.e',member='M',path='/otro',path_namespace='/n',destination=':1.8',arg0='a',arg0namespace='n'",
            "type='signal',sender=':1.9',interface='org.e',member='M',path='/p',path_namespace='/otro',destination=':1.8',arg0='a',arg0namespace='n'",
            "type='signal',sender=':1.9',interface='org.e',member='M',path='/p',path_namespace='/n',destination=':1.7',arg0='a',arg0namespace='n'",
            "type='signal',sender=':1.9',interface='org.e',member='M',path='/p',path_namespace='/n',destination=':1.8',arg0='otro',arg0namespace='n'",
            "type='signal',sender=':1.9',interface='org.e',member='M',path='/p',path_namespace='/n',destination=':1.8',arg0='a',arg0namespace='otra'",
            // Unsupported is a field too: a rule the daemon cannot express
            // selects NOTHING, so it is not the same rule as one that works.
            "type='signal',sender=':1.9',interface='org.e',member='M',path='/p',path_namespace='/n',destination=':1.8',arg0='a',arg0namespace='n',desconocida='x'",
        ] {
            let mut b = bus();
            hello(&mut b, 1);
            bus_call(&mut b, 1, 2, "AddMatch", base);
            take(&mut b);
            bus_call(&mut b, 1, 3, "RemoveMatch", other);
            let out = take(&mut b);
            assert_eq!(
                out[0].1.kind,
                MSG_ERROR,
                "quito la regla con una distinta: {other}"
            );
            assert_eq!(b.conn(1).unwrap().rules.len(), 1, "{other}");
            // And the rule itself still comes off with its own text.
            bus_call(&mut b, 1, 4, "RemoveMatch", base);
            let out = take(&mut b);
            assert_eq!(out[0].1.kind, MSG_METHOD_RETURN, "{other}");
        }
    }

    #[test]
    fn an_empty_dictionary_reply_is_eight_bytes_not_four() {
        // `GetConnectionCredentials` and `GetAll` answer `a{sv}`, whose body is
        // the length word AND the padding to the element's alignment: a
        // DICT_ENTRY is 8-aligned, so an empty one is eight bytes. The daemon
        // wrote four, four short of where a reader following the spec stops,
        // while the crate's own `Arg::DictSS` writer has always emitted the
        // padding -- the same array, two answers.
        let mut b = bus();
        hello(&mut b, 1);
        for (iface, member) in [
            (DBUS_NAME, "GetConnectionCredentials"),
            ("org.freedesktop.DBus.Properties", "GetAll"),
        ] {
            let mut m = Message::method_call(DBUS_NAME, DBUS_PATH, iface, member);
            m.serial = 2;
            m.set_body(&[Arg::Str(":1.1".to_string())]);
            b.dispatch(1, m);
            let out = take(&mut b);
            let r = &out[0].1;
            assert_eq!(r.kind, MSG_METHOD_RETURN, "{member}: {:?}", r.error_name);
            assert_eq!(r.signature.as_deref(), Some("a{sv}"), "{member}");
            assert_eq!(r.body.len(), 8, "{member}: {:?}", r.body);
            assert_eq!(r.body, vec![0u8; 8], "{member}");
            // And it is byte-for-byte what the crate's own writer produces for
            // the same shape, which is the disagreement that let this through.
            let mut twin = Message::signal("/", "i", "M");
            twin.set_body(&[Arg::DictSS(Vec::new())]);
            assert_eq!(r.body, twin.body, "{member}: writer y mano a mano difieren");
        }
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

    /// Call `member` on `iface` (empty string for the bus's own interface) and
    /// give back the one message the bus sends to the caller.
    fn call(b: &mut Bus, id: u64, serial: u32, iface: &str, member: &str, args: &[Arg]) -> Message {
        let mut m = Message::method_call(DBUS_NAME, DBUS_PATH, iface, member);
        m.serial = serial;
        if !args.is_empty() {
            m.set_body(args);
        }
        b.dispatch(id, m);
        let mut mine: Vec<Message> = take(b)
            .into_iter()
            .filter(|(to, _)| *to == id)
            .map(|(_, m)| m)
            .collect();
        assert_eq!(mine.len(), 1, "{member} answered {} times", mine.len());
        mine.remove(0)
    }

    fn err_name(m: &Message) -> &str {
        assert_eq!(m.kind, MSG_ERROR, "expected an error, got kind {}", m.kind);
        m.error_name.as_deref().unwrap_or("")
    }

    /// Every row of the bus's own method table answers what that row
    /// promises. Two thirds of the table had no test at all: a reply wired to
    /// the wrong field, or an arm that quietly answered "no such method",
    /// looked exactly like a client bug from the outside.
    #[test]
    fn every_bus_method_answers_with_what_its_own_row_promises() {
        let mut b = bus();
        hello(&mut b, 1);
        hello(&mut b, 2);
        bus_call(&mut b, 2, 10, "RequestName", "org.example.Owner");
        take(&mut b);

        // The bus GUID and the machine id are two different strings, and a
        // client that only ever asks for one cannot tell them apart.
        let r = call(&mut b, 1, 20, "", "GetId", &[]);
        assert_eq!(r.args(), vec![Arg::Str("abc".into())]);
        let r = call(
            &mut b,
            1,
            21,
            "org.freedesktop.DBus.Peer",
            "GetMachineId",
            &[],
        );
        assert_eq!(
            r.args(),
            vec![Arg::Str("0123456789abcdef0123456789abcdef".into())]
        );

        // Introspection answers with the document, not with an empty string:
        // a client that gets "" believes the bus has no methods.
        let r = call(
            &mut b,
            1,
            22,
            "org.freedesktop.DBus.Introspectable",
            "Introspect",
            &[],
        );
        match r.args().first() {
            Some(Arg::Str(xml)) => {
                assert!(xml.contains("<node>"), "introspection is a document");
                assert!(xml.contains("name=\"Hello\""));
            }
            other => panic!("Introspect answered {other:?}"),
        }

        // `Properties.Get` takes (interface, property): the PROPERTY is the
        // second argument, so reading the first one answers about the
        // interface name and refuses every real property.
        for prop in ["Features", "Interfaces"] {
            let r = call(
                &mut b,
                1,
                30,
                "org.freedesktop.DBus.Properties",
                "Get",
                &[Arg::Str(DBUS_NAME.into()), Arg::Str(prop.into())],
            );
            assert_eq!(r.kind, MSG_METHOD_RETURN, "Get {prop}");
            assert_eq!(
                r.args(),
                vec![Arg::Variant(Box::new(Arg::StrArray(Vec::new())))],
                "Get {prop}"
            );
        }
        let r = call(
            &mut b,
            1,
            31,
            "org.freedesktop.DBus.Properties",
            "Get",
            &[Arg::Str(DBUS_NAME.into()), Arg::Str("Nope".into())],
        );
        assert_eq!(err_name(&r), "org.freedesktop.DBus.Error.UnknownProperty");
        // Nothing on the bus is writable, and saying so is not the same as
        // saying "not supported".
        let r = call(
            &mut b,
            1,
            32,
            "org.freedesktop.DBus.Properties",
            "Set",
            &[Arg::Str(DBUS_NAME.into()), Arg::Str("Features".into())],
        );
        assert_eq!(err_name(&r), "org.freedesktop.DBus.Error.PropertyReadOnly");

        // Activation is not implemented, so the only always-available name is
        // the bus itself -- and the list is not empty, which is what a client
        // reads as "this bus activates nothing, not even itself".
        let r = call(&mut b, 1, 40, "", "ListActivatableNames", &[]);
        assert_eq!(r.args(), vec![Arg::StrArray(vec![DBUS_NAME.to_string()])]);

        // `NameHasOwner` has to say yes to the bus's own name as well as to a
        // name a client owns: the bus is not in `names`, so the two halves of
        // that answer come from different places.
        for (name, has) in [
            (DBUS_NAME, true),
            ("org.example.Owner", true),
            (":1.2", true),
            ("org.example.Nobody", false),
        ] {
            let r = call(&mut b, 1, 41, "", "NameHasOwner", &[Arg::Str(name.into())]);
            assert_eq!(r.args(), vec![Arg::Bool(has)], "NameHasOwner {name}");
        }

        // The owner of a name is the connection that owns it, not the one
        // that asked.
        let r = call(
            &mut b,
            1,
            42,
            "",
            "GetNameOwner",
            &[Arg::Str("org.example.Owner".into())],
        );
        assert_eq!(r.args(), vec![Arg::Str(":1.2".into())]);
        let r = call(
            &mut b,
            1,
            43,
            "",
            "GetNameOwner",
            &[Arg::Str(DBUS_NAME.into())],
        );
        assert_eq!(r.args(), vec![Arg::Str(DBUS_NAME.into())]);
        let r = call(
            &mut b,
            1,
            44,
            "",
            "GetNameOwner",
            &[Arg::Str("org.example.Nobody".into())],
        );
        assert_eq!(err_name(&r), ERR_NAME_HAS_NO_OWNER);

        // The credentials of the owner of a name. The uid and the pid are two
        // different numbers here on purpose: a test with both at zero cannot
        // tell the two replies apart.
        let r = call(
            &mut b,
            1,
            50,
            "",
            "GetConnectionUnixUser",
            &[Arg::Str("org.example.Owner".into())],
        );
        assert_eq!(r.args(), vec![Arg::U32(0)]);
        let r = call(
            &mut b,
            1,
            51,
            "",
            "GetConnectionUnixProcessID",
            &[Arg::Str("org.example.Owner".into())],
        );
        assert_eq!(r.args(), vec![Arg::U32(102)]);
        let r = call(
            &mut b,
            1,
            52,
            "",
            "GetConnectionUnixUser",
            &[Arg::Str("org.example.Nobody".into())],
        );
        assert_eq!(err_name(&r), ERR_NAME_HAS_NO_OWNER);

        // A name that is already there is "already running"; one that is not
        // cannot be started, because nothing here starts services.
        let r = call(
            &mut b,
            1,
            60,
            "",
            "StartServiceByName",
            &[Arg::Str("org.example.Owner".into())],
        );
        assert_eq!(
            r.args(),
            vec![Arg::U32(2)],
            "DBUS_START_REPLY_ALREADY_RUNNING"
        );
        let r = call(
            &mut b,
            1,
            61,
            "",
            "StartServiceByName",
            &[Arg::Str("org.example.Nobody".into())],
        );
        assert_eq!(err_name(&r), ERR_SERVICE_UNKNOWN);

        // Accepted and ignored: a client that gets an error here gives up.
        for member in ["UpdateActivationEnvironment", "ReloadConfig"] {
            let r = call(&mut b, 1, 70, "", member, &[]);
            assert_eq!(r.kind, MSG_METHOD_RETURN, "{member}");
            assert!(r.args().is_empty(), "{member}");
        }
        // Refused, and both of them: an arm that answers only one leaves the
        // other on the "no such method" path, which is a different error.
        for member in [
            "GetAdtAuditSessionData",
            "GetConnectionSELinuxSecurityContext",
        ] {
            let r = call(&mut b, 1, 71, "", member, &[Arg::Str(":1.2".into())]);
            assert_eq!(err_name(&r), ERR_NOT_SUPPORTED, "{member}");
        }
    }

    /// The introspection document and the method table have to say the same
    /// thing. They are two hand-written lists of the same set, so nothing but
    /// a test holds them together: a method dropped from the document is a
    /// method a client never tries, and one advertised but not answered is a
    /// client that gets "no such method" after introspecting for it.
    #[test]
    fn the_introspection_document_and_the_method_table_agree() {
        let xml = introspection_xml();
        for m in [
            "Hello",
            "RequestName",
            "ReleaseName",
            "ListNames",
            "ListActivatableNames",
            "NameHasOwner",
            "GetNameOwner",
            "ListQueuedOwners",
            "GetConnectionUnixUser",
            "GetConnectionUnixProcessID",
            "AddMatch",
            "RemoveMatch",
            "GetId",
            "StartServiceByName",
            "Ping",
            "GetMachineId",
            "Introspect",
        ] {
            assert!(
                xml.contains(&format!("name=\"{m}\"")),
                "{m} is answered but not advertised"
            );
        }

        let mut b = bus();
        hello(&mut b, 1);
        let mut serial = 100;
        let mut seen = 0;
        for line in xml.lines() {
            let line = line.trim();
            let rest = match line.strip_prefix("<method name=\"") {
                Some(r) => r,
                None => continue,
            };
            let member = rest.split('"').next().unwrap();
            let iface = match member {
                "Ping" | "GetMachineId" => "org.freedesktop.DBus.Peer",
                "Introspect" => "org.freedesktop.DBus.Introspectable",
                _ => "",
            };
            serial += 1;
            let mut m = Message::method_call(DBUS_NAME, DBUS_PATH, iface, member);
            m.serial = serial;
            b.dispatch(1, m);
            for (_, out) in take(&mut b) {
                assert_ne!(
                    out.error_name.as_deref(),
                    Some(ERR_UNKNOWN_METHOD),
                    "{member} is advertised but not answered"
                );
            }
            seen += 1;
        }
        assert_eq!(
            seen, 17,
            "the document advertises exactly the seventeen methods listed above"
        );
    }

    /// A well-known name is checked against every rule the specification
    /// gives, and the two refusals a client can act on say which it was. The
    /// test before this one tried one bad name; the rules are six.
    #[test]
    fn a_well_known_name_is_checked_against_every_rule_the_specification_has() {
        for (name, ok) in [
            ("org.example.Foo", true),
            // Both of the extra characters a name may use.
            ("org.example.Foo-Bar_2", true),
            ("a.b", true),
            ("", false),
            ("nodot", false),
            (".leading", false),
            ("trailing.", false),
            ("org..double", false),
            // A digit may appear, but not first in an element.
            ("org.9nine", false),
            ("9nine.org", false),
            ("org.n9ine", true),
            ("org.has space", false),
            ("org.has/slash", false),
            ("org.has.dollar$", false),
        ] {
            assert_eq!(
                validate_well_known_name(name).is_ok(),
                ok,
                "validating {name:?}"
            );
        }

        // 255 bytes is a ceiling, so 256 is over it.
        let at = format!("a.{}", "b".repeat(253));
        assert_eq!(at.len(), 255);
        assert!(validate_well_known_name(&at).is_ok());
        let over = format!("a.{}", "b".repeat(254));
        assert_eq!(over.len(), 256);
        assert!(validate_well_known_name(&over).is_err());

        // Asking for something only the bus hands out is a different mistake
        // from asking for a malformed name, and the client is told which.
        let e = validate_well_known_name(":1.5").unwrap_err();
        assert!(e.contains("unique name"), "{e}");
        let e = validate_well_known_name(DBUS_NAME).unwrap_err();
        assert!(e.contains("the bus's own name"), "{e}");
    }

    /// A connection that has not said `Hello` has no identity yet, so it is
    /// not in `ListNames` -- and the well-known names are, which is the other
    /// half of the list.
    #[test]
    fn the_name_list_holds_the_bus_the_registered_and_the_well_known() {
        let mut b = bus();
        let u1 = hello(&mut b, 1);
        bus_call(&mut b, 1, 5, "RequestName", "org.example.Listed");
        // Connected, never said Hello: it has a `:1.N` but nobody may use it.
        b.add_connection(9, 109, 0);
        take(&mut b);

        let r = call(&mut b, 1, 6, "", "ListNames", &[]);
        let names = match r.args().first() {
            Some(Arg::StrArray(v)) => v.clone(),
            other => panic!("ListNames answered {other:?}"),
        };
        assert!(names.contains(&DBUS_NAME.to_string()), "{names:?}");
        assert!(names.contains(&u1), "{names:?}");
        assert!(
            names.contains(&"org.example.Listed".to_string()),
            "the well-known names belong in the list: {names:?}"
        );
        let silent = b.conn(9).unwrap().unique.clone();
        assert!(
            !names.contains(&silent),
            "a connection that never said Hello is not in the list: {names:?}"
        );
    }

    /// `ListQueuedOwners` is the primary owner first and then the queue in
    /// the order it was joined. An answer that drops either half is an
    /// answer, so nothing fails -- the client just cannot see who is waiting.
    #[test]
    fn the_queued_owners_are_the_owner_and_then_the_queue_in_order() {
        let mut b = bus();
        hello(&mut b, 1);
        hello(&mut b, 2);
        hello(&mut b, 3);
        for id in [1u64, 2, 3] {
            bus_call(&mut b, id, 10, "RequestName", "org.example.Q");
        }
        // Asking again from the back of the queue must not buy a second
        // place: a connection listed twice is handed the name twice, the
        // second time after it had already given it up.
        bus_call(&mut b, 2, 11, "RequestName", "org.example.Q");
        take(&mut b);

        let r = call(
            &mut b,
            1,
            11,
            "",
            "ListQueuedOwners",
            &[Arg::Str("org.example.Q".into())],
        );
        assert_eq!(
            r.args(),
            vec![Arg::StrArray(vec![
                ":1.1".into(),
                ":1.2".into(),
                ":1.3".into()
            ])]
        );
        let r = call(
            &mut b,
            1,
            12,
            "",
            "ListQueuedOwners",
            &[Arg::Str("org.example.Nobody".into())],
        );
        assert_eq!(err_name(&r), ERR_NAME_HAS_NO_OWNER);
    }

    /// The waiting queue is a queue: the name goes to the connection that has
    /// been waiting longest, and a connection that asks twice while waiting
    /// does not get two places in it.
    #[test]
    fn the_name_queue_hands_over_in_the_order_it_was_joined() {
        let mut b = bus();
        for id in [1u64, 2, 3] {
            hello(&mut b, id);
        }
        bus_call(&mut b, 1, 10, "RequestName", "org.example.Q");
        bus_call(&mut b, 2, 10, "RequestName", "org.example.Q");
        bus_call(&mut b, 3, 10, "RequestName", "org.example.Q");
        // Asking again from the back of the queue must not buy a second place.
        bus_call(&mut b, 2, 11, "RequestName", "org.example.Q");
        take(&mut b);

        bus_call(&mut b, 1, 12, "ReleaseName", "org.example.Q");
        take(&mut b);
        let r = call(
            &mut b,
            1,
            13,
            "",
            "GetNameOwner",
            &[Arg::Str("org.example.Q".into())],
        );
        assert_eq!(
            r.args(),
            vec![Arg::Str(":1.2".into())],
            "the oldest waiter takes the name"
        );
        // And exactly one place each, so the next release reaches :1.3.
        bus_call(&mut b, 2, 14, "ReleaseName", "org.example.Q");
        take(&mut b);
        let r = call(
            &mut b,
            1,
            15,
            "",
            "GetNameOwner",
            &[Arg::Str("org.example.Q".into())],
        );
        assert_eq!(r.args(), vec![Arg::Str(":1.3".into())]);
    }

    /// A displaced owner goes to the FRONT of the queue, so it gets its name
    /// back before anybody who was already waiting. dbus-daemon does the
    /// same, and a service restarted under a client that takes the name over
    /// depends on it.
    #[test]
    fn a_displaced_owner_is_first_in_line_for_its_name_again() {
        let mut b = bus();
        for id in [1u64, 2, 3] {
            hello(&mut b, id);
        }
        // :1.1 owns it and allows replacement; :1.2 queues behind it.
        request(&mut b, 1, "org.example.R", NAME_FLAG_ALLOW_REPLACEMENT);
        request(&mut b, 2, "org.example.R", 0);
        // :1.3 takes it over, which puts :1.1 at the head of the queue.
        assert_eq!(
            request(&mut b, 3, "org.example.R", NAME_FLAG_REPLACE_EXISTING),
            REQUEST_NAME_PRIMARY_OWNER
        );
        take(&mut b);

        bus_call(&mut b, 3, 20, "ReleaseName", "org.example.R");
        take(&mut b);
        let r = call(
            &mut b,
            1,
            21,
            "",
            "GetNameOwner",
            &[Arg::Str("org.example.R".into())],
        );
        assert_eq!(
            r.args(),
            vec![Arg::Str(":1.1".into())],
            "the displaced owner comes before the waiter"
        );
    }

    /// `RequestName` with `flags`, giving back the code the bus answers with.
    fn request(b: &mut Bus, id: u64, name: &str, flags: u32) -> u32 {
        let mut m = Message::method_call(DBUS_NAME, DBUS_PATH, DBUS_NAME, "RequestName");
        m.serial = 99;
        m.set_body(&[Arg::Str(name.to_string()), Arg::U32(flags)]);
        b.dispatch(id, m);
        let out = take(b);
        let reply = out
            .iter()
            .find(|(to, m)| *to == id && m.kind == MSG_METHOD_RETURN)
            .map(|(_, m)| m.clone())
            .expect("RequestName was answered");
        match reply.args().first() {
            Some(Arg::U32(c)) => *c,
            other => panic!("RequestName answered {other:?}"),
        }
    }

    /// Taking a name over needs BOTH sides to agree: the owner must have said
    /// its name may be replaced AND the challenger must ask to replace it.
    /// Either half alone leaves the owner where it is.
    #[test]
    fn a_name_is_taken_over_only_when_both_sides_agree() {
        for (owner_flags, taker_flags, taken) in [
            (
                NAME_FLAG_ALLOW_REPLACEMENT,
                NAME_FLAG_REPLACE_EXISTING,
                true,
            ),
            // The owner allows it but nobody asked.
            (NAME_FLAG_ALLOW_REPLACEMENT, 0, false),
            // Asked for, but the owner never allowed it.
            (0, NAME_FLAG_REPLACE_EXISTING, false),
            (0, 0, false),
        ] {
            let mut b = bus();
            hello(&mut b, 1);
            hello(&mut b, 2);
            assert_eq!(
                request(&mut b, 1, "org.example.T", owner_flags),
                REQUEST_NAME_PRIMARY_OWNER
            );
            let code = request(&mut b, 2, "org.example.T", taker_flags);
            if taken {
                assert_eq!(
                    code, REQUEST_NAME_PRIMARY_OWNER,
                    "{owner_flags}/{taker_flags}"
                );
            } else {
                assert_eq!(code, REQUEST_NAME_IN_QUEUE, "{owner_flags}/{taker_flags}");
            }
        }
    }

    /// `ReleaseName` has three outcomes and a client acts on each: the name
    /// was yours and is gone, the name is not yours, or there is no such
    /// name. Only the first had a test.
    #[test]
    fn release_name_tells_its_three_outcomes_apart() {
        let mut b = bus();
        hello(&mut b, 1);
        hello(&mut b, 2);
        request(&mut b, 1, "org.example.Held", 0);
        // :1.2 waits for it, so it may release its PLACE IN THE QUEUE.
        assert_eq!(
            request(&mut b, 2, "org.example.Held", 0),
            REQUEST_NAME_IN_QUEUE
        );
        take(&mut b);

        assert_eq!(
            release(&mut b, 1, "org.example.Nothing"),
            RELEASE_NAME_NON_EXISTENT
        );
        assert_eq!(
            release(&mut b, 2, "org.example.Held"),
            RELEASE_NAME_RELEASED,
            "a waiter releases its place in the queue"
        );
        // And having left the queue, it is no longer anything to the name.
        assert_eq!(
            release(&mut b, 2, "org.example.Held"),
            RELEASE_NAME_NOT_OWNER
        );
        assert_eq!(
            release(&mut b, 1, "org.example.Held"),
            RELEASE_NAME_RELEASED
        );
        assert_eq!(
            release(&mut b, 1, "org.example.Held"),
            RELEASE_NAME_NON_EXISTENT,
            "the last owner leaving takes the name with it"
        );
    }

    fn release(b: &mut Bus, id: u64, name: &str) -> u32 {
        let mut m = Message::method_call(DBUS_NAME, DBUS_PATH, DBUS_NAME, "ReleaseName");
        m.serial = 98;
        m.set_body(&[Arg::Str(name.to_string())]);
        b.dispatch(id, m);
        let out = take(b);
        let reply = out
            .iter()
            .find(|(to, m)| *to == id && m.kind == MSG_METHOD_RETURN)
            .map(|(_, m)| m.clone())
            .expect("ReleaseName was answered");
        match reply.args().first() {
            Some(Arg::U32(c)) => *c,
            other => panic!("ReleaseName answered {other:?}"),
        }
    }

    /// A connection that goes away comes out of the queues it was merely
    /// waiting in, not only off the names it owned. Left behind, it is handed
    /// the name when the owner releases it -- a name owned by a connection
    /// that no longer exists, which nothing can take back.
    #[test]
    fn a_connection_that_leaves_comes_out_of_every_queue() {
        let mut b = bus();
        hello(&mut b, 1);
        hello(&mut b, 2);
        request(&mut b, 1, "org.example.G", 0);
        request(&mut b, 2, "org.example.G", 0);
        take(&mut b);

        b.remove_connection(2);
        take(&mut b);
        assert_eq!(release(&mut b, 1, "org.example.G"), RELEASE_NAME_RELEASED);
        let r = call(
            &mut b,
            1,
            30,
            "",
            "GetNameOwner",
            &[Arg::Str("org.example.G".into())],
        );
        assert_eq!(
            err_name(&r),
            ERR_NAME_HAS_NO_OWNER,
            "the name is free, not owned by a connection that left"
        );
    }

    /// A connection that never said `Hello` has no identity on the bus, so
    /// nothing is announced when it goes: `NameOwnerChanged` for a `:1.N` no
    /// client was ever told about is a name appearing and vanishing out of
    /// nowhere.
    #[test]
    fn a_connection_that_never_said_hello_leaves_without_a_word() {
        let mut b = bus();
        hello(&mut b, 1);
        bus_call(
            &mut b,
            1,
            5,
            "AddMatch",
            "type='signal',member='NameOwnerChanged'",
        );
        take(&mut b);

        b.add_connection(9, 109, 0);
        b.remove_connection(9);
        assert!(
            take(&mut b).is_empty(),
            "a connection nobody was told about leaves silently"
        );

        // Whereas one that did say Hello is announced as gone.
        hello(&mut b, 8);
        take(&mut b);
        b.remove_connection(8);
        let out = take(&mut b);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, 1);
        assert_eq!(out[0].1.member.as_deref(), Some("NameOwnerChanged"));
    }

    /// The bus's serial counter skips zero when it wraps: serial 0 is
    /// reserved by the specification and a client that receives it treats the
    /// message as corrupt.
    #[test]
    fn the_bus_serial_never_lands_on_zero() {
        let mut b = bus();
        b.serial = u32::MAX;
        assert_eq!(b.next_serial(), 1, "it wraps past zero, not onto it");
        assert_eq!(b.next_serial(), 2);
    }

    /// Only a `Hello` that is a METHOD CALL addressed to the BUS gets a
    /// connection past the handshake. A signal called Hello, or a call sent
    /// to somebody else, is an unregistered client's traffic and the bus
    /// refuses it -- otherwise a client could register without the bus ever
    /// answering, and then route with an identity nobody announced.
    #[test]
    fn only_a_method_call_to_the_bus_counts_as_the_hello() {
        // A signal named Hello.
        let mut b = bus();
        b.add_connection(1, 101, 0);
        let mut sig = Message::signal(DBUS_PATH, DBUS_NAME, "Hello");
        sig.serial = 1;
        sig.destination = Some(DBUS_NAME.to_string());
        b.dispatch(1, sig);
        let out = take(&mut b);
        assert_eq!(out.len(), 1);
        assert_eq!(err_name(&out[0].1), ERR_ACCESS_DENIED);
        assert!(!b.conn(1).unwrap().hello);

        // A Hello call addressed to another name.
        let mut b = bus();
        b.add_connection(1, 101, 0);
        let mut m = Message::method_call("org.example.Other", DBUS_PATH, DBUS_NAME, "Hello");
        m.serial = 1;
        b.dispatch(1, m);
        let out = take(&mut b);
        assert_eq!(out.len(), 1);
        assert_eq!(err_name(&out[0].1), ERR_ACCESS_DENIED);
        assert!(!b.conn(1).unwrap().hello);
    }

    /// A monitor's rule may name a sender by a WELL-KNOWN name, so the copy
    /// has to be matched against the names the sender owns and not only
    /// against its `:1.N`. The sender's identity reaches `monitor_copy` as an
    /// argument; drop it and every such rule silently matches nothing.
    #[test]
    fn a_monitor_rule_that_names_a_well_known_sender_sees_its_traffic() {
        let mut b = bus();
        hello(&mut b, 1);
        hello(&mut b, 2);
        request(&mut b, 2, "org.example.Speaker", 0);
        let mut m = Message::method_call(DBUS_NAME, DBUS_PATH, DBUS_NAME, "BecomeMonitor");
        m.serial = 5;
        m.set_body(&[Arg::StrArray(vec![
            "sender='org.example.Speaker'".to_string()
        ])]);
        b.dispatch(1, m);
        take(&mut b);

        example_signal(&mut b, 2, 6);
        let out = take(&mut b);
        assert_eq!(
            out.len(),
            1,
            "the monitor sees the owner's signal through its well-known name: {out:?}"
        );
        assert_eq!(out[0].0, 1);
    }

    /// Becoming a monitor gives up everything the connection was: the names
    /// it owned go to whoever was waiting, and its ordinary subscriptions are
    /// dropped so it does not receive a message twice -- once as the monitor
    /// copy and once as a subscriber.
    #[test]
    fn a_monitor_gives_up_its_names_and_its_subscriptions() {
        let mut b = bus();
        hello(&mut b, 1);
        hello(&mut b, 2);
        hello(&mut b, 3);
        request(&mut b, 1, "org.example.Was", 0);
        request(&mut b, 3, "org.example.Was", 0);
        bus_call(
            &mut b,
            1,
            5,
            "AddMatch",
            "type='signal',interface='org.example'",
        );
        take(&mut b);

        let mut m = Message::method_call(DBUS_NAME, DBUS_PATH, DBUS_NAME, "BecomeMonitor");
        m.serial = 6;
        b.dispatch(1, m);
        take(&mut b);

        // The waiter has the name now.
        let r = call(
            &mut b,
            2,
            7,
            "",
            "GetNameOwner",
            &[Arg::Str("org.example.Was".into())],
        );
        assert_eq!(
            r.args(),
            vec![Arg::Str(":1.3".into())],
            "a monitor owns nothing"
        );

        // And one copy of a matching signal, not two.
        example_signal(&mut b, 2, 8);
        let out = take(&mut b);
        assert_eq!(
            out.len(),
            1,
            "exactly one copy reaches the monitor: {out:?}"
        );
        assert_eq!(out[0].0, 1);
    }

    /// Giving a name up takes it off the connection that had it. The list a
    /// connection carries is what a `sender=` rule is matched against, so a
    /// stale entry has a watcher receiving traffic from a connection that
    /// gave the name away -- under a name it no longer owns.
    #[test]
    fn giving_a_name_up_takes_it_off_the_connection_that_had_it() {
        let mut b = bus();
        hello(&mut b, 1);
        hello(&mut b, 2);
        hello(&mut b, 3);
        request(&mut b, 2, "org.example.Mover", 0);
        request(&mut b, 3, "org.example.Mover", 0);
        bus_call(&mut b, 1, 5, "AddMatch", "sender='org.example.Mover'");
        take(&mut b);

        assert_eq!(
            release(&mut b, 2, "org.example.Mover"),
            RELEASE_NAME_RELEASED
        );
        assert_eq!(
            b.conn(2).unwrap().names,
            Vec::<String>::new(),
            "the name is off the old owner's list"
        );

        // :1.2 no longer owns it, so its traffic no longer matches.
        example_signal(&mut b, 2, 6);
        assert!(
            take(&mut b).is_empty(),
            "the watcher follows the name, not the connection that had it"
        );
        // And the new owner's traffic does.
        example_signal(&mut b, 3, 7);
        let out = take(&mut b);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, 1);
    }

    /// A unicast call reaches its destination once, even when the
    /// destination's own rules also match it: the broadcast skips the
    /// connection that is about to receive the message directly.
    #[test]
    fn the_addressee_of_a_call_receives_it_exactly_once() {
        let mut b = bus();
        hello(&mut b, 1);
        hello(&mut b, 2);
        request(&mut b, 2, "org.example.Dest", 0);
        bus_call(&mut b, 2, 5, "AddMatch", "type='method_call'");
        take(&mut b);

        let mut m = Message::method_call("org.example.Dest", "/org/a", "org.example", "Do");
        m.serial = 6;
        b.dispatch(1, m);
        let out = take(&mut b);
        assert_eq!(out.len(), 1, "one copy, not two: {out:?}");
        assert_eq!(out[0].0, 2);
    }

    /// `RemoveMatch` removes the rule the client named, not whichever rule is
    /// first. A connection usually holds several, and taking the wrong one
    /// away leaves the client subscribed to traffic it asked to stop and
    /// deaf to traffic it still wants.
    #[test]
    fn remove_match_takes_away_the_rule_it_was_given() {
        let mut b = bus();
        hello(&mut b, 1);
        hello(&mut b, 2);
        bus_call(
            &mut b,
            1,
            5,
            "AddMatch",
            "type='signal',interface='org.example'",
        );
        bus_call(
            &mut b,
            1,
            6,
            "AddMatch",
            "type='signal',interface='org.other'",
        );
        take(&mut b);

        bus_call(
            &mut b,
            1,
            7,
            "RemoveMatch",
            "type='signal',interface='org.other'",
        );
        let out = take(&mut b);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1.kind, MSG_METHOD_RETURN, "the rule was there");

        // The rule that was not named is still in force.
        example_signal(&mut b, 2, 8);
        let out = take(&mut b);
        assert_eq!(out.len(), 1, "the other rule survived: {out:?}");
        assert_eq!(out[0].0, 1);
        // And the one that was named is gone.
        let mut sig = Message::signal("/org/a", "org.other", "Ping");
        sig.serial = 9;
        b.dispatch(2, sig);
        assert!(take(&mut b).is_empty(), "the named rule is gone");
    }
}
