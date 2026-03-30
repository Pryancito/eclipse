use crate::wl::server::objects::{Object, ObjectInner, DisplayObject, ServerError};
use crate::wl::protocols::common::wl_display::WlDisplay;
use crate::wl::{ObjectId, NewId, Connection, Message, interface::InterfaceWrapper};
use alloc::rc::Rc;
use core::cell::RefCell;
use alloc::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClientId(pub u32);

pub struct Client {
    client_id: ClientId,
    connection: Rc<RefCell<dyn Connection>>,
    objects: BTreeMap<ObjectId, Object>,
    serial: u32,
}

impl Client {
    pub fn new(client_id: ClientId, connection: Rc<RefCell<dyn Connection>>) -> Client {
        let mut objects = BTreeMap::new();
        let display = ObjectInner::Rc(Rc::new(RefCell::new(DisplayObject)));
        objects.insert(ObjectId(1), Object::new::<WlDisplay>(NewId(1), display));
        Client {
            client_id,
            connection,
            objects,
            serial: 1,
        }
    }

    pub fn client_id(&self) -> ClientId {
        self.client_id
    }

    pub fn connection(&self) -> &Rc<RefCell<dyn Connection>> {
        &self.connection
    }

    pub fn object_mut(&mut self, id: ObjectId) -> Result<&mut Object, ServerError> {
        self.objects.get_mut(&id).ok_or(ServerError::UnknownObjectId(id))
    }

    pub fn send_event<E: Message>(&self, object: ObjectId, event: E) -> Result<(), ServerError> {
        let raw = event.into_raw(object);
        self.connection.borrow_mut().send(raw.sender, raw.opcode, &raw.args, &[]).map_err(|_| ServerError::IoError)
    }

    pub fn add_object(&mut self, id: NewId, object: Object) {
        self.objects.insert(id.as_id(), object);
    }
}
