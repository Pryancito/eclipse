use crate::wl::{ObjectId, NewId, Interface, RawMessage, Connection, Payload};
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
    fn handle_request(&mut self, client: &mut Client, opcode: u16, args: &[Payload]) -> Result<(), ServerError> {
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
    fn handle_request(&mut self, client: &mut Client, opcode: u16, args: &[Payload]) -> Result<(), ServerError> {
        match opcode {
            0 => { // bind
                let name = match args[0] { Payload::UInt(n) => n, _ => return Err(ServerError::MessageDeserializeError) };
                let interface = match &args[1] { Payload::String(s) => s, _ => return Err(ServerError::MessageDeserializeError) };
                let version = match args[2] { Payload::UInt(v) => v, _ => return Err(ServerError::MessageDeserializeError) };
                let id = match args[3] { Payload::NewId(id) => id, _ => return Err(ServerError::MessageDeserializeError) };
                
                // We'll need a way to look up the global factory here.
                Ok(())
            }
            _ => Ok(()),
        }
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
