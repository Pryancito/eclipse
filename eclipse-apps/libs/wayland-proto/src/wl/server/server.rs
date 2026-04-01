use crate::wl::server::client::{Client, ClientId};
use crate::wl::server::objects::{Object, ObjectInner, ServerError};
use crate::wl::{ObjectId, NewId, Message};
use crate::wl::connection::Connection;
use crate::wl::wire::RawMessage;
use core::cell::RefCell;
use alloc::rc::Rc;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

pub struct Global {
    pub name: u32,
    pub interface: &'static str,
    pub version: u32,
    pub logic_factory: alloc::boxed::Box<dyn Fn() -> ObjectInner>,
    pub interface_type: fn(NewId, ObjectInner) -> Object,
    /// Optional callback invoked immediately after a client binds this global.
    /// Use it to send initial events (e.g. `wl_seat.capabilities`,
    /// `wl_output.geometry/mode/done`).
    pub post_bind: Option<alloc::boxed::Box<dyn Fn(ObjectId, &mut Client) -> Result<(), ServerError>>>,
}

pub struct WaylandServer {
    pub clients: BTreeMap<ClientId, Client>,
    pub globals: Vec<Global>,
    next_global_id: u32,
}

impl WaylandServer {
    pub fn new() -> Self {
        Self {
            clients: BTreeMap::new(),
            globals: Vec::new(),
            next_global_id: 1,
        }
    }

    pub fn register_global(
        &mut self,
        interface: &'static str,
        version: u32,
        logic_factory: impl Fn() -> ObjectInner + 'static,
        interface_type: fn(NewId, ObjectInner) -> Object,
    ) {
        self.register_global_with_post_bind(interface, version, logic_factory, interface_type, None);
    }

    /// Like [`register_global`] but also calls `post_bind` right after a
    /// client successfully binds the global, allowing initial events to be sent.
    pub fn register_global_with_post_bind(
        &mut self,
        interface: &'static str,
        version: u32,
        logic_factory: impl Fn() -> ObjectInner + 'static,
        interface_type: fn(NewId, ObjectInner) -> Object,
        post_bind: Option<alloc::boxed::Box<dyn Fn(ObjectId, &mut Client) -> Result<(), ServerError>>>,
    ) {
        let name = self.next_global_id;
        self.next_global_id += 1;
        self.globals.push(Global {
            name,
            interface,
            version,
            logic_factory: alloc::boxed::Box::new(logic_factory),
            interface_type,
            post_bind,
        });
    }

    pub fn add_client(&mut self, id: ClientId, con: Rc<RefCell<dyn Connection>>) {
        self.clients.insert(id, Client::new(id, con));
    }

    pub fn process_message(
        &mut self,
        client_id: ClientId,
        message: &[u8],
    ) -> Result<(), ServerError> {
        let (object_id, opcode, _len) = RawMessage::deserialize_header(message)
            .map_err(|_| ServerError::MessageDeserializeError)?;

        let (inner, interface_name, payload_types) = {
            let client = self.clients.get_mut(&client_id).ok_or(ServerError::ClientNotFound)?;
            let object = client.object_mut(object_id)?;
            let interface = object.interface();
            let payload_types = interface.payload_types(opcode.0);
            let interface_name = interface.name();
            let inner = match object.inner() {
                crate::wl::server::objects::ObjectInner::Boxed(_) => return Err(ServerError::ObjectMismatch), // We only support Rc for now
                crate::wl::server::objects::ObjectInner::Rc(rc) => rc.clone(),
            };
            (inner, interface_name, payload_types)
        };

        let raw = RawMessage::deserialize(message, payload_types, &[])
            .map_err(|_| ServerError::MessageDeserializeError)?;

        let mut res = Ok(());
        {
            let client = self.clients.get_mut(&client_id).ok_or(ServerError::ClientNotFound)?;
            res = inner.borrow_mut().handle_request(client, opcode.0, &raw.args);
        }

        // Intercept get_registry on display (id=1, opcode=2) to broadcast globals
        if object_id == crate::wl::ObjectId(1) && opcode.0 == 2 {
             let registry_id = match raw.args[0] {
                  crate::wl::Payload::NewId(id) => id.as_id(),
                  _ => return Err(ServerError::MessageDeserializeError),
             };
             let client = self.clients.get_mut(&client_id).ok_or(ServerError::ClientNotFound)?;
             for global in &self.globals {
                  let event = crate::wl::protocols::common::wl_registry::Event::Global {
                       name: global.name,
                       interface: alloc::string::String::from(global.interface),
                       version: global.version,
                  };
                  client.send_event(registry_id, event)?;
             }
        }

        // Intercept bind on registry (opcode=0) to instantiate objects from globals
        if interface_name == "wl_registry" && opcode.0 == 0 {
             let name = match raw.args[0] { crate::wl::Payload::UInt(n) => n, _ => return Err(ServerError::MessageDeserializeError) };
             let id = match raw.args[3] { crate::wl::Payload::NewId(id) => id, _ => return Err(ServerError::MessageDeserializeError) };

             let global_idx = self.globals.iter().position(|g| g.name == name)
                  .ok_or(ServerError::UnknownGlobal)?;

             // Call logic_factory and interface_type using index (temporary borrows).
             let logic = (self.globals[global_idx].logic_factory)();
             let new_obj = (self.globals[global_idx].interface_type)(id, logic);

             // Add the new object to the client.
             let client = self.clients.get_mut(&client_id).ok_or(ServerError::ClientNotFound)?;
             client.add_object(id, new_obj);

             // Invoke post_bind callback if present.
             // Borrow self.globals immutably (different field from self.clients).
             if let Some(ref cb) = self.globals[global_idx].post_bind {
                  let _ = cb(id.as_id(), client);
             }
        }

        res
    }
}
