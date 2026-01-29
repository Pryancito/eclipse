//! Wayland Server Core
//!
//! Implements the main Wayland server that manages clients and dispatches protocol messages

use crate::protocol::*;
use crate::objects::*;
use heapless::Vec;

/// Maximum number of clients
pub const MAX_CLIENTS: usize = 16;

/// Maximum number of objects per client
pub const MAX_OBJECTS: usize = 256;

/// Client connection state
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ClientState {
    Connected,
    Disconnected,
    Error,
}

/// Wayland client representation
pub struct Client {
    pub id: u32,
    pub state: ClientState,
    pub socket_fd: i32,
    pub objects: Vec<WaylandObject, MAX_OBJECTS>,
    pub next_object_id: u32,
}

impl Client {
    pub fn new(id: u32, socket_fd: i32) -> Self {
        let mut client = Self {
            id,
            state: ClientState::Connected,
            socket_fd,
            objects: Vec::new(),
            next_object_id: 2, // 1 is reserved for wl_display
        };
        
        // Create wl_display object
        let _ = client.objects.push(WaylandObject::new(
            1,
            InterfaceType::Display,
            ObjectState::Active,
        ));
        
        client
    }

    pub fn allocate_object_id(&mut self) -> u32 {
        let id = self.next_object_id;
        self.next_object_id += 1;
        id
    }

    pub fn add_object(&mut self, obj: WaylandObject) -> Result<(), &'static str> {
        self.objects.push(obj).map_err(|_| "Too many objects")
    }

    pub fn get_object(&self, id: u32) -> Option<&WaylandObject> {
        self.objects.iter().find(|obj| obj.id == id)
    }

    pub fn get_object_mut(&mut self, id: u32) -> Option<&mut WaylandObject> {
        self.objects.iter_mut().find(|obj| obj.id == id)
    }

    pub fn remove_object(&mut self, id: u32) -> bool {
        if let Some(pos) = self.objects.iter().position(|obj| obj.id == id) {
            self.objects.swap_remove(pos);
            true
        } else {
            false
        }
    }
}

/// Global registry entries
pub struct RegistryGlobal {
    pub name: u32,
    pub interface: InterfaceType,
    pub version: u32,
}

/// Main Wayland server
pub struct WaylandServer {
    pub clients: Vec<Client, MAX_CLIENTS>,
    pub globals: Vec<RegistryGlobal, 16>,
    pub next_client_id: u32,
    pub next_global_name: u32,
    pub running: bool,
}

impl WaylandServer {
    pub fn new() -> Self {
        let mut server = Self {
            clients: Vec::new(),
            globals: Vec::new(),
            next_client_id: 1,
            next_global_name: 1,
            running: false,
        };
        
        // Register core globals
        server.register_global(InterfaceType::Compositor, 4);
        server.register_global(InterfaceType::Shm, 1);
        server.register_global(InterfaceType::Seat, 5);
        server.register_global(InterfaceType::Output, 3);
        server.register_global(InterfaceType::Shell, 1);
        
        server
    }

    pub fn register_global(&mut self, interface: InterfaceType, version: u32) {
        let name = self.next_global_name;
        self.next_global_name += 1;
        
        let _ = self.globals.push(RegistryGlobal {
            name,
            interface,
            version,
        });
        // Note: In production, failing to register core globals should be a critical error
    }

    pub fn add_client(&mut self, socket_fd: i32) -> Result<u32, &'static str> {
        let id = self.next_client_id;
        self.next_client_id += 1;
        
        let client = Client::new(id, socket_fd);
        self.clients.push(client).map_err(|_| "Too many clients")?;
        
        Ok(id)
    }

    pub fn get_client(&self, id: u32) -> Option<&Client> {
        self.clients.iter().find(|c| c.id == id)
    }

    pub fn get_client_mut(&mut self, id: u32) -> Option<&mut Client> {
        self.clients.iter_mut().find(|c| c.id == id)
    }

    pub fn remove_client(&mut self, id: u32) -> bool {
        if let Some(pos) = self.clients.iter().position(|c| c.id == id) {
            self.clients.swap_remove(pos);
            true
        } else {
            false
        }
    }

    /// Process a message from a client
    pub fn process_message(&mut self, client_id: u32, msg: &Message) -> Result<(), &'static str> {
        let client = self.get_client_mut(client_id)
            .ok_or("Client not found")?;

        // Get the object interface type (copy it to avoid borrow conflicts)
        let interface = client.get_object(msg.header.object_id)
            .ok_or("Object not found")?
            .interface;

        match interface {
            InterfaceType::Display => self.handle_display_message(client_id, msg),
            InterfaceType::Registry => self.handle_registry_message(client_id, msg),
            InterfaceType::Compositor => self.handle_compositor_message(client_id, msg),
            InterfaceType::Surface => self.handle_surface_message(client_id, msg),
            _ => Ok(()), // Not implemented yet
        }
    }

    fn handle_display_message(&mut self, client_id: u32, msg: &Message) -> Result<(), &'static str> {
        match msg.header.opcode() {
            0 => { // sync
                // Send done event
                Ok(())
            }
            1 => { // get_registry
                // Create registry object
                if let Some(client) = self.get_client_mut(client_id) {
                    let registry_id = client.allocate_object_id();
                    let registry = WaylandObject::new(
                        registry_id,
                        InterfaceType::Registry,
                        ObjectState::Active,
                    );
                    client.add_object(registry)?;
                    
                    // Send global events for all registered globals
                    // (In a real implementation, we'd send these over the socket)
                }
                Ok(())
            }
            _ => Err("Unknown display opcode"),
        }
    }

    fn handle_registry_message(&mut self, client_id: u32, msg: &Message) -> Result<(), &'static str> {
        match msg.header.opcode() {
            0 => { // bind
                // Extract arguments: name, interface, version, new_id
                // Create the bound object
                Ok(())
            }
            _ => Err("Unknown registry opcode"),
        }
    }

    fn handle_compositor_message(&mut self, client_id: u32, msg: &Message) -> Result<(), &'static str> {
        match msg.header.opcode() {
            0 => { // create_surface
                if let Some(client) = self.get_client_mut(client_id) {
                    let surface_id = client.allocate_object_id();
                    let surface = WaylandObject::new(
                        surface_id,
                        InterfaceType::Surface,
                        ObjectState::Active,
                    );
                    client.add_object(surface)?;
                }
                Ok(())
            }
            1 => { // create_region
                Ok(())
            }
            _ => Err("Unknown compositor opcode"),
        }
    }

    fn handle_surface_message(&mut self, client_id: u32, msg: &Message) -> Result<(), &'static str> {
        match msg.header.opcode() {
            0 => Ok(()), // destroy
            1 => Ok(()), // attach
            2 => Ok(()), // damage
            3 => Ok(()), // frame
            4 => Ok(()), // set_opaque_region
            5 => Ok(()), // set_input_region
            6 => Ok(()), // commit
            _ => Err("Unknown surface opcode"),
        }
    }
}
