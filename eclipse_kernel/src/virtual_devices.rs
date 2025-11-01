//! Dispositivos Virtuales de Ejemplo para Eclipse OS
//!
//! Este módulo contiene implementaciones de dispositivos virtuales que demuestran
//! el funcionamiento del sistema de dispositivos del kernel.

use super::devices::*;
use alloc::boxed::Box;
use alloc::vec::Vec;
use alloc::vec;
use alloc::string::{String, ToString};

// Función simple de logging
fn device_info(_message: &str) {
    // Logging simplificado - no hacer nada por ahora
}

fn device_debug(_message: &str) {
    // Logging simplificado - no hacer nada por ahora
}

/// Dispositivo de almacenamiento virtual (RAM disk)
pub struct VirtualStorageDevice {
    info: DeviceInfo,
    data: Vec<u8>,
    capacity: usize,
}

impl VirtualStorageDevice {
    /// Crea un nuevo dispositivo de almacenamiento virtual
    pub fn new(name: &str, capacity: usize) -> Self {
        let mut info = DeviceInfo::new(DeviceId::new(), name, DeviceType::Storage);
        info.base_address = Some(0); // No mapeado en memoria real
        info.driver_version = String::from("1.0.0");

        Self {
            info,
            data: vec![0; capacity],
            capacity,
        }
    }
}

impl Device for VirtualStorageDevice {
    fn init(&mut self) -> DeviceResult<()> {
        self.info.state = DeviceState::Ready;
        device_info("Dispositivo de almacenamiento virtual inicializado");
        Ok(())
    }

    fn shutdown(&mut self) -> DeviceResult<()> {
        self.info.state = DeviceState::Uninitialized;
        device_info("Dispositivo de almacenamiento virtual apagado");
        Ok(())
    }

    fn get_info(&self) -> &DeviceInfo {
        &self.info
    }

    fn get_info_mut(&mut self) -> &mut DeviceInfo {
        &mut self.info
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
}

impl StorageDevice for VirtualStorageDevice {
    fn read(&mut self, offset: u64, buffer: &mut [u8]) -> DeviceResult<usize> {
        if self.info.state != DeviceState::Ready {
            return Err(DeviceError::NotInitialized);
        }

        let offset = offset as usize;
        if offset >= self.capacity {
            return Err(DeviceError::IoError(String::from("Offset fuera de rango")));
        }

        let available = self.capacity - offset;
        let to_read = core::cmp::min(buffer.len(), available);

        buffer[..to_read].copy_from_slice(&self.data[offset..offset + to_read]);
        Ok(to_read)
    }

    fn write(&mut self, offset: u64, data: &[u8]) -> DeviceResult<usize> {
        if self.info.state != DeviceState::Ready {
            return Err(DeviceError::NotInitialized);
        }

        let offset = offset as usize;
        if offset >= self.capacity {
            return Err(DeviceError::IoError(String::from("Offset fuera de rango")));
        }

        let available = self.capacity - offset;
        let to_write = core::cmp::min(data.len(), available);

        self.data[offset..offset + to_write].copy_from_slice(&data[..to_write]);
        Ok(to_write)
    }

    fn size(&self) -> u64 {
        self.capacity as u64
    }

    fn flush(&mut self) -> DeviceResult<()> {
        // En un dispositivo virtual, flush es un no-op
        Ok(())
    }
}

/// Dispositivo de entrada virtual (genera datos aleatorios)
pub struct VirtualInputDevice {
    info: DeviceInfo,
    seed: u32,
}

impl VirtualInputDevice {
    /// Crea un nuevo dispositivo de entrada virtual
    pub fn new(name: &str) -> Self {
        let mut info = DeviceInfo::new(DeviceId::new(), name, DeviceType::Input);
        info.driver_version = String::from("1.0.0");

        Self {
            info,
            seed: 12345, // Semilla simple para "aleatoriedad"
        }
    }

    /// Genera un número pseudo-aleatorio simple
    fn next_random(&mut self) -> u8 {
        self.seed = self.seed.wrapping_mul(1103515245).wrapping_add(12345);
        (self.seed >> 16) as u8
    }
}

impl Device for VirtualInputDevice {
    fn init(&mut self) -> DeviceResult<()> {
        self.info.state = DeviceState::Ready;
        // Logging disabled
        Ok(())
    }

    fn shutdown(&mut self) -> DeviceResult<()> {
        self.info.state = DeviceState::Uninitialized;
        // Logging disabled
        Ok(())
    }

    fn get_info(&self) -> &DeviceInfo {
        &self.info
    }

    fn get_info_mut(&mut self) -> &mut DeviceInfo {
        &mut self.info
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
}

impl InputDevice for VirtualInputDevice {
    fn read_input(&mut self, buffer: &mut [u8]) -> DeviceResult<usize> {
        if self.info.state != DeviceState::Ready {
            return Err(DeviceError::NotInitialized);
        }

        // Generar datos "aleatorios" para simular entrada
        for i in 0..buffer.len() {
            buffer[i] = self.next_random();
        }

        device_debug("Device debug");
        Ok(buffer.len())
    }

    fn has_data(&self) -> bool {
        // Un dispositivo virtual siempre tiene datos disponibles
        self.info.state == DeviceState::Ready
    }
}

/// Dispositivo de salida virtual (almacena datos en buffer)
pub struct VirtualOutputDevice {
    info: DeviceInfo,
    buffer: Vec<u8>,
    max_buffer_size: usize,
}

impl VirtualOutputDevice {
    /// Crea un nuevo dispositivo de salida virtual
    pub fn new(name: &str, max_buffer_size: usize) -> Self {
        let mut info = DeviceInfo::new(DeviceId::new(), name, DeviceType::Output);
        info.driver_version = String::from("1.0.0");

        Self {
            info,
            buffer: Vec::new(),
            max_buffer_size,
        }
    }

    /// Obtiene el contenido actual del buffer
    pub fn get_buffer_contents(&self) -> &[u8] {
        &self.buffer
    }

    /// Limpia el buffer
    pub fn clear_buffer(&mut self) {
        self.buffer.clear();
    }
}

impl Device for VirtualOutputDevice {
    fn init(&mut self) -> DeviceResult<()> {
        self.info.state = DeviceState::Ready;
        Ok(())
    }

    fn shutdown(&mut self) -> DeviceResult<()> {
        self.info.state = DeviceState::Uninitialized;
        self.buffer.clear();
        // Logging disabled
        Ok(())
    }

    fn get_info(&self) -> &DeviceInfo {
        &self.info
    }

    fn get_info_mut(&mut self) -> &mut DeviceInfo {
        &mut self.info
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
}

impl OutputDevice for VirtualOutputDevice {
    fn write_output(&mut self, data: &[u8]) -> DeviceResult<usize> {
        if self.info.state != DeviceState::Ready {
            return Err(DeviceError::NotInitialized);
        }

        let available_space = self.max_buffer_size - self.buffer.len();
        let to_write = core::cmp::min(data.len(), available_space);

        if to_write == 0 {
            return Err(DeviceError::Busy); // Buffer lleno
        }

        self.buffer.extend_from_slice(&data[..to_write]);

        device_debug("Device debug");
        Ok(to_write)
    }
}

/// Dispositivo de red virtual (loopback)
pub struct VirtualNetworkDevice {
    info: DeviceInfo,
    mac_address: [u8; 6],
    ip_address: Option<[u8; 4]>,
    rx_buffer: Vec<u8>,
    tx_buffer: Vec<u8>,
}

impl VirtualNetworkDevice {
    /// Crea un nuevo dispositivo de red virtual
    pub fn new(name: &str) -> Self {
        let mut info = DeviceInfo::new(DeviceId::new(), name, DeviceType::Network);
        info.driver_version = String::from("1.0.0");

        Self {
            info,
            mac_address: [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF], // MAC virtual
            ip_address: Some([192, 168, 1, 100]), // IP virtual
            rx_buffer: Vec::new(),
            tx_buffer: Vec::new(),
        }
    }

    /// Simula la recepción de un paquete (loopback)
    pub fn simulate_packet_receive(&mut self, data: &[u8]) {
        self.rx_buffer.extend_from_slice(data);
        device_debug("Device debug");
    }
}

impl Device for VirtualNetworkDevice {
    fn init(&mut self) -> DeviceResult<()> {
        self.info.state = DeviceState::Ready;
        device_info("Device operation");
        Ok(())
    }

    fn shutdown(&mut self) -> DeviceResult<()> {
        self.info.state = DeviceState::Uninitialized;
        self.rx_buffer.clear();
        self.tx_buffer.clear();
        // Logging disabled
        Ok(())
    }

    fn get_info(&self) -> &DeviceInfo {
        &self.info
    }

    fn get_info_mut(&mut self) -> &mut DeviceInfo {
        &mut self.info
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
}

impl NetworkDevice for VirtualNetworkDevice {
    fn send_packet(&mut self, data: &[u8]) -> DeviceResult<usize> {
        if self.info.state != DeviceState::Ready {
            return Err(DeviceError::NotInitialized);
        }

        // En un dispositivo virtual, simplemente almacenamos el paquete
        self.tx_buffer.extend_from_slice(data);

        Ok(data.len())
    }

    fn receive_packet(&mut self, buffer: &mut [u8]) -> DeviceResult<usize> {
        if self.info.state != DeviceState::Ready {
            return Err(DeviceError::NotInitialized);
        }

        if self.rx_buffer.is_empty() {
            return Ok(0); // No hay datos disponibles
        }

        let to_read = core::cmp::min(buffer.len(), self.rx_buffer.len());
        buffer[..to_read].copy_from_slice(&self.rx_buffer[..to_read]);

        // Remover los datos leídos del buffer
        self.rx_buffer.drain(0..to_read);

        Ok(to_read)
    }

    fn get_mac_address(&self) -> [u8; 6] {
        self.mac_address
    }

    fn get_ip_address(&self) -> Option<[u8; 4]> {
        self.ip_address
    }
}

/// Función para crear y registrar dispositivos virtuales de demostración
pub fn create_demo_devices() -> DeviceResult<()> {
    // Logging disabled

    // Crear dispositivo de almacenamiento virtual
    let storage_device = Box::new(VirtualStorageDevice::new("vramdisk", 1024 * 1024)); // 1MB
    register_device_global(storage_device)?;

    // Crear dispositivo de entrada virtual
    let input_device = Box::new(VirtualInputDevice::new("vinput"));
    register_device_global(input_device)?;

    // Crear dispositivo de salida virtual
    let output_device = Box::new(VirtualOutputDevice::new("voutput", 4096)); // 4KB buffer
    register_device_global(output_device)?;

    // Crear dispositivo de red virtual
    let network_device = Box::new(VirtualNetworkDevice::new("vnet"));
    register_device_global(network_device)?;

    Ok(())
}

/// Función de demostración que muestra el uso del sistema de dispositivos
pub fn demo_device_usage() -> DeviceResult<()> {
    let manager = get_device_manager().ok_or(DeviceError::Other(String::from("Manager no disponible")))?;

    // Listar todos los dispositivos
    let devices = manager.list_devices();
    for device in &devices {
        // Procesar dispositivos
    }

    // Demostrar uso del dispositivo de almacenamiento
    if let Some(storage_id) = devices.iter().find(|d| d.device_type == DeviceType::Storage).map(|d| d.id) {
        if let Some(storage) = manager.get_device_as_mut::<VirtualStorageDevice>(storage_id) {
            // Escribir datos
            let data = b"Hola, mundo desde Eclipse OS!";
            let _written = storage.write(0, data)?;

            // Leer datos
            let mut _buffer = [0u8; 32];
            let _read = storage.read(0, &mut _buffer)?;
        }
    }

    // Demostrar uso del dispositivo de entrada
    if let Some(input_id) = devices.iter().find(|d| d.device_type == DeviceType::Input).map(|d| d.id) {
        if let Some(input) = manager.get_device_as_mut::<VirtualInputDevice>(input_id) {
            let mut _buffer = [0u8; 16];
            let _read = input.read_input(&mut _buffer)?;
        }
    }

    // Demostrar uso del dispositivo de salida
    if let Some(output_id) = devices.iter().find(|d| d.device_type == DeviceType::Output).map(|d| d.id) {
        if let Some(output) = manager.get_device_as_mut::<VirtualOutputDevice>(output_id) {
            let message = b"Mensaje de prueba para dispositivo de salida";
            let _written = output.write_output(message)?;

            let _contents = output.get_buffer_contents();
        }
    }

    // Demostrar uso del dispositivo de red
    if let Some(network_id) = devices.iter().find(|d| d.device_type == DeviceType::Network).map(|d| d.id) {
        if let Some(network) = manager.get_device_as_mut::<VirtualNetworkDevice>(network_id) {
            let _mac = network.get_mac_address();

            // Simular envío y recepción de paquete
            let packet = b"Paquete de prueba";
            network.simulate_packet_receive(packet);

            let mut _buffer = [0u8; 32];
            let _received = network.receive_packet(&mut _buffer)?;
        }
    }

    // Mostrar estadísticas finales
    let _stats = manager.get_stats();
    Ok(())
}
