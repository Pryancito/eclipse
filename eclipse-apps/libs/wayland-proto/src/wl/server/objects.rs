use crate::wl::{ObjectId, NewId, Interface, RawMessage, Connection, Payload};
use crate::wl::wire::Handle;
use crate::wl::interface::{construct_interface_wrapper, InterfaceWrapper};
use crate::wl::server::client::Client;
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
            1 => { // get_registry
                let registry_id = match args[0] {
                    Payload::NewId(id) => id,
                    _ => return Err(ServerError::MessageDeserializeError),
                };
                
                // We need access to server.globals here... 
                // But wait, the client belongs to the server.
                // For now, let's just create the registry object.
                // The actual global broadcasting should probably happen in WaylandServer.
                
                let registry = ObjectInner::Rc(Rc::new(RefCell::new(RegistryObject)));
                client.add_object(registry_id, Object::new::<crate::wl::protocols::common::wl_registry::WlRegistry>(registry_id, registry));
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

#[derive(Debug)]
pub enum ServerError {
    ClientNotFound,
    ObjectMismatch,
    UnknownGlobal,
    IoError,
    MessageDeserializeError,
    UnknownObjectId(ObjectId),
}
