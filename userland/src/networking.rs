//! Networking Module
//! Redes y conectividad

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::net::{TcpStream, UdpSocket, IpAddr, Ipv4Addr, SocketAddr};
use std::io::{Read, Write};

/// Estado de conexión
#[derive(Debug, Clone, PartialEq)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Error,
}

/// Tipo de conexión
#[derive(Debug, Clone)]
pub enum ConnectionType {
    Tcp,
    Udp,
}

/// Estructura de red
struct NetworkInternal {
    state: ConnectionState,
    interface: String,
    tcp_stream: Option<TcpStream>,
    udp_socket: Option<UdpSocket>,
    connection_type: ConnectionType,
    local_addr: Option<SocketAddr>,
    remote_addr: Option<SocketAddr>,
    buffer: Vec<u8>,
}

/// Handle de red
pub struct NetworkHandle {
    internal: Arc<Mutex<NetworkInternal>>,
}

impl Default for NetworkHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl NetworkHandle {
    fn new() -> Self {
        NetworkHandle {
            internal: Arc::new(Mutex::new(NetworkInternal {
                state: ConnectionState::Disconnected,
                interface: String::new(),
                tcp_stream: None,
                udp_socket: None,
                connection_type: ConnectionType::Tcp,
                local_addr: None,
                remote_addr: None,
                buffer: Vec::new(),
            })),
        }
    }
}

/// Inicializar red
pub fn Network_Initialize() {
    println!("🌐 Red inicializada");
}

/// Crear instancia de red
pub fn create_network() -> NetworkHandle {
    NetworkHandle::new()
}

/// Configurar interfaz de red
pub fn configure_network_interface(network: &NetworkHandle, interface: &str) -> bool {
    if let Ok(mut internal) = network.internal.lock() {
        internal.interface = interface.to_string();
        println!("🌐 Interfaz de red configurada: {}", interface);
        true
    } else {
        false
    }
}

/// Conectar a servidor TCP
pub fn connect_to_server(network: &NetworkHandle, host: &str, port: u16) -> bool {
    if let Ok(mut internal) = network.internal.lock() {
        let addr = format!("{}:{}", host, port);
        
        match TcpStream::connect(&addr) {
            Ok(stream) => {
                println!("🌐 Conectado a servidor TCP: {}", addr);
                internal.tcp_stream = Some(stream);
                internal.state = ConnectionState::Connected;
                internal.connection_type = ConnectionType::Tcp;
                true
            }
            Err(e) => {
                eprintln!("❌ Error conectando a {}: {}", addr, e);
                internal.state = ConnectionState::Error;
                false
            }
        }
    } else {
        false
    }
}

/// Conectar a red (WiFi simulado)
pub fn connect_to_network(network: &NetworkHandle, ssid: &str, password: &str) -> bool {
    if let Ok(mut internal) = network.internal.lock() {
        internal.state = ConnectionState::Connecting;
        
        // Simular conexión WiFi
        println!("🌐 Conectando a red WiFi: {}", ssid);
        println!("   Autenticando con contraseña...");
        
        if !password.is_empty() {
            internal.state = ConnectionState::Connected;
            println!("✅ Conectado a red: {}", ssid);
            true
        } else {
            eprintln!("❌ Contraseña inválida");
            internal.state = ConnectionState::Error;
            false
        }
    } else {
        false
    }
}

/// Desconectar de red
pub fn disconnect_from_network(network: &NetworkHandle) -> bool {
    if let Ok(mut internal) = network.internal.lock() {
        internal.tcp_stream = None;
        internal.udp_socket = None;
        internal.state = ConnectionState::Disconnected;
        println!("🌐 Desconectado de red");
        true
    } else {
        false
    }
}

/// Enviar datos
pub fn send_data(network: &NetworkHandle, data: &[u8]) -> bool {
    if let Ok(mut internal) = network.internal.lock() {
        if internal.state != ConnectionState::Connected {
            eprintln!("❌ No hay conexión activa");
            return false;
        }
        
        match &mut internal.tcp_stream {
            Some(stream) => {
                match stream.write_all(data) {
                    Ok(_) => {
                        println!("📤 Datos enviados: {} bytes", data.len());
                        true
                    }
                    Err(e) => {
                        eprintln!("❌ Error enviando datos: {}", e);
                        false
                    }
                }
            }
            None => {
                // Guardar en buffer si no hay conexión TCP activa
                internal.buffer.extend_from_slice(data);
                println!("📦 Datos guardados en buffer: {} bytes", data.len());
                true
            }
        }
    } else {
        false
    }
}

/// Recibir datos
pub fn receive_data(network: &NetworkHandle) -> Vec<u8> {
    if let Ok(mut internal) = network.internal.lock() {
        if internal.state != ConnectionState::Connected {
            return vec![];
        }
        
        match &mut internal.tcp_stream {
            Some(stream) => {
                let mut buffer = vec![0u8; 4096];
                match stream.read(&mut buffer) {
                    Ok(size) if size > 0 => {
                        buffer.truncate(size);
                        println!("📥 Datos recibidos: {} bytes", size);
                        buffer
                    }
                    Ok(_) => vec![],
                    Err(e) => {
                        eprintln!("❌ Error recibiendo datos: {}", e);
                        vec![]
                    }
                }
            }
            None => {
                // Retornar datos del buffer si no hay conexión TCP
                let data = internal.buffer.clone();
                internal.buffer.clear();
                data
            }
        }
    } else {
        vec![]
    }
}

/// Obtener estado de red
pub fn get_network_status(network: &NetworkHandle) -> String {
    if let Ok(internal) = network.internal.lock() {
        match internal.state {
            ConnectionState::Disconnected => "desconectado".to_string(),
            ConnectionState::Connecting => "conectando...".to_string(),
            ConnectionState::Connected => "conectado".to_string(),
            ConnectionState::Error => "error".to_string(),
        }
    } else {
        "desconocido".to_string()
    }
}

/// Crear socket UDP
pub fn create_udp_socket(network: &NetworkHandle, port: u16) -> bool {
    if let Ok(mut internal) = network.internal.lock() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), port);
        
        match UdpSocket::bind(addr) {
            Ok(socket) => {
                println!("🌐 Socket UDP creado en puerto {}", port);
                internal.udp_socket = Some(socket);
                internal.connection_type = ConnectionType::Udp;
                internal.state = ConnectionState::Connected;
                true
            }
            Err(e) => {
                eprintln!("❌ Error creando socket UDP: {}", e);
                false
            }
        }
    } else {
        false
    }
}

/// Enviar datos UDP
pub fn send_udp_data(network: &NetworkHandle, data: &[u8], dest: &str, port: u16) -> bool {
    if let Ok(internal) = network.internal.lock() {
        match &internal.udp_socket {
            Some(socket) => {
                let addr = format!("{}:{}", dest, port);
                match socket.send_to(data, &addr) {
                    Ok(size) => {
                        println!("📤 Datos UDP enviados a {}: {} bytes", addr, size);
                        true
                    }
                    Err(e) => {
                        eprintln!("❌ Error enviando datos UDP: {}", e);
                        false
                    }
                }
            }
            None => {
                eprintln!("❌ No hay socket UDP activo");
                false
            }
        }
    } else {
        false
    }
}

/// Obtener información de red
pub fn get_network_info(network: &NetworkHandle) -> (String, String, String) {
    if let Ok(internal) = network.internal.lock() {
        let state = format!("{:?}", internal.state);
        let conn_type = format!("{:?}", internal.connection_type);
        let interface = internal.interface.clone();
        
        (state, conn_type, interface)
    } else {
        ("Unknown".to_string(), "Unknown".to_string(), "Unknown".to_string())
    }
}

/// Liberar red
pub fn free_network(_network: &mut NetworkHandle) -> bool {
    // Se libera automáticamente al salir del scope
    println!("🌐 Red liberada");
    true
}