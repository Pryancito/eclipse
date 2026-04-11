use crate::wl::{ObjectId, NewId, Interface, RawMessage, Connection, Payload};
use crate::wl::wire::Handle;
use crate::wl::interface::{construct_interface_wrapper, InterfaceWrapper};
use crate::wl::server::client::Client;
use crate::wl::protocols::common::wl_callback::WlCallback;
use crate::wl::protocols::common::wl_display;
use crate::wl::protocols::common::wl_registry::WlRegistry;
use alloc::boxed::Box;
use alloc::rc::Rc;
use core::cell::RefCell;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GlobalObjectId(pub u32);

pub trait ObjectLogic: 'static {
    fn handle_request(
        &mut self,
        client: &mut Client,
        opcode: u16,
        args: &[Payload],
        handles: &[Handle],
    ) -> Result<(), ServerError>;
}

pub enum ObjectInner {
    Boxed(Box<dyn ObjectLogic>),
    Rc(Rc<RefCell<dyn ObjectLogic>>),
}

pub struct Object {
    id: ObjectId,
    interface: Box<dyn InterfaceWrapper>,
    inner: ObjectInner,
}

impl Object {
    pub fn new<I: Interface>(id: NewId, inner: ObjectInner) -> Object {
        Object {
            id: id.as_id(),
            interface: construct_interface_wrapper::<I>(),
            inner,
        }
    }

    pub fn id(&self) -> ObjectId {
        self.id
    }

    pub fn interface(&self) -> &dyn InterfaceWrapper {
        &*self.interface
    }

    pub fn inner(&self) -> &ObjectInner {
        &self.inner
    }
}

pub struct DisplayObject;
impl ObjectLogic for DisplayObject {
    fn handle_request(&mut self, client: &mut Client, opcode: u16, args: &[Payload], _handles: &[Handle]) -> Result<(), ServerError> {
        match opcode {
            0 => {
                // sync(callback: new_id) — create a wl_callback, fire Done immediately,
                // then delete the object so the client can recycle the ID.
                // This is how wl_display_roundtrip() knows the server has caught up.
                let callback_id = match args.first() {
                    Some(Payload::NewId(id)) => *id,
                    _ => return Err(ServerError::MessageDeserializeError),
                };
                let cb_inner = ObjectInner::Rc(Rc::new(RefCell::new(CallbackObject)));
                client.add_object(callback_id, Object::new::<WlCallback>(callback_id, cb_inner));
                // Fire Done — clients use this to detect the roundtrip has completed.
                client.send_event(
                    callback_id.as_id(),
                    crate::wl::protocols::common::wl_callback::Event::Done { callback_data: 0 },
                )?;
                // Delete the one-shot object so the client can reuse the ID.
                client.send_event(
                    ObjectId(1),
                    wl_display::Event::DeleteId { id: callback_id.0 },
                )?;
                Ok(())
            }
            1 => { // get_registry
                let registry_id = match args.first() {
                    Some(Payload::NewId(id)) => *id,
                    _ => return Err(ServerError::MessageDeserializeError),
                };
                let registry = ObjectInner::Rc(Rc::new(RefCell::new(RegistryObject)));
                client.add_object(registry_id, Object::new::<WlRegistry>(registry_id, registry));
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

pub struct RegistryObject;
impl ObjectLogic for RegistryObject {
    fn handle_request(&mut self, _client: &mut Client, _opcode: u16, _args: &[Payload], _handles: &[Handle]) -> Result<(), ServerError> {
        // bind is handled directly by WaylandServer::process_message; nothing to do here.
        Ok(())
    }
}

/// No-op handler for wl_callback objects.
/// The compositor fires `wl_callback.done` externally; the object itself has no requests.
pub struct CallbackObject;
impl ObjectLogic for CallbackObject {
    fn handle_request(&mut self, _client: &mut Client, _opcode: u16, _args: &[Payload], _handles: &[Handle]) -> Result<(), ServerError> {
        Ok(())
    }
}

#[derive(Debug)]
pub enum ServerError {
    ClientNotFound,
    ObjectMismatch,
    UnknownGlobal,
    IoError,
    MessageDeserializeError,
    UnknownObjectId(ObjectId),
}
