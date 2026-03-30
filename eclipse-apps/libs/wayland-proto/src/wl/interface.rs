use crate::wl::{Connection, ObjectId, Payload, PayloadType, NewId, Message};
use alloc::rc::Rc;
use core::cell::RefCell;
use core::marker::PhantomData;

pub trait Interface: 'static {
    type Event: Message;
    type Request: Message;

    /// The interface name.
    const NAME: &'static str;
    /// The interface version.
    const VERSION: u32;
    /// Payload types for each opcode.
    const PAYLOAD_TYPES: &'static [&'static [PayloadType]];

    fn new(con: Rc<RefCell<dyn Connection>>, id: ObjectId) -> Self;
    fn connection(&self) -> &Rc<RefCell<dyn Connection>>;
    fn id(&self) -> ObjectId;
    fn as_new_id(&self) -> NewId;
}

pub trait InterfaceWrapper {
    fn name(&self) -> &'static str;
    fn version(&self) -> u32;
    fn payload_types(&self, opcode: u16) -> &'static [PayloadType];
}

pub struct GenericInterfaceWrapper<I: Interface> {
    _marker: PhantomData<I>,
}

impl<I: Interface> InterfaceWrapper for GenericInterfaceWrapper<I> {
    fn name(&self) -> &'static str {
        I::NAME
    }
    fn version(&self) -> u32 {
        I::VERSION
    }
    fn payload_types(&self, opcode: u16) -> &'static [PayloadType] {
        I::PAYLOAD_TYPES[opcode as usize]
    }
}

pub fn construct_interface_wrapper<I: Interface>() -> alloc::boxed::Box<dyn InterfaceWrapper> {
    alloc::boxed::Box::new(GenericInterfaceWrapper::<I> { _marker: PhantomData })
}

impl<I: Interface> From<Option<I>> for Payload {
    fn from(opt: Option<I>) -> Self {
        match opt {
            Some(object) => Payload::ObjectId(object.id()),
            None => Payload::ObjectId(ObjectId::null()),
        }
    }
}
