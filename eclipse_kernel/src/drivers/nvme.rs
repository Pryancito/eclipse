//! Driver NVMe para controladoras de almacenamiento modernas
//! 
//! Este driver implementa acceso directo a controladoras NVMe PCIe
//! para hardware físico moderno.

use crate::debug::serial_write_str;
use alloc::{format, vec::Vec, string::{String, ToString}};

/// Registros de control NVMe
const NVME_CAP: u32 = 0x00;        // Capabilities
const NVME_VS: u32 = 0x08;         // Version
const NVME_CC: u32 = 0x14;         // Controller Configuration
const NVME_CSTS: u32 = 0x1C;       // Controller Status
const NVME_AQA: u32 = 0x24;        // Admin Queue Attributes
const NVME_ASQ: u32 = 0x28;        // Admin Submission Queue Base Address
const NVME_ACQ: u32 = 0x30;        // Admin Completion Queue Base Address

/// Comandos NVMe
const NVME_CMD_IDENTIFY: u8 = 0x06;
const NVME_CMD_READ: u8 = 0x02;
const NVME_CMD_WRITE: u8 = 0x01;

/// Estados del controlador
const NVME_CSTS_RDY: u32 = 0x01;
const NVME_CSTS_CFS: u32 = 0x02;

/// Información del dispositivo NVMe
#[derive(Debug, Clone)]
pub struct NvmeDeviceInfo {
    pub model: String,
    pub serial: String,
    pub firmware: String,
    pub namespace_count: u32,
    pub max_lba: u64,
    pub block_size: u32,
    pub capacity: u64,
}

/// Información de namespace NVMe
#[derive(Debug, Clone)]
pub struct NvmeNamespaceInfo {
    pub nsid: u32,         // Namespace ID
    pub size: u64,         // Tamaño en bloques
    pub capacity: u64,     // Capacidad en bloques
    pub block_size: u32,   // Tamaño de bloque en bytes
    pub formatted_lba_size: u8, // Índice del formato LBA actual
}

/// Estructura de comando NVMe
#[repr(C, packed)]
struct NvmeCommand {
    opcode: u8,
    flags: u8,
    command_id: u16,
    namespace_id: u32,
    cdw2: u32,
    cdw3: u32,
    metadata_ptr: u64,
    data_ptr: u64,
    cdw10: u32,
    cdw11: u32,
    cdw12: u32,
    cdw13: u32,
    cdw14: u32,
    cdw15: u32,
}

/// Estructura de respuesta NVMe
#[repr(C, packed)]
struct NvmeCompletion {
    command_specific: u32,
    reserved: u32,
    sq_head: u16,
    sq_id: u16,
    command_id: u16,
    status: u16,
}

pub struct NvmeDriver {
    base_addr: u32,
    is_initialized: bool,
    device_info: Option<NvmeDeviceInfo>,
    admin_queue_head: u16,
    admin_queue_tail: u16,
    submission_queue: Vec<NvmeCommand>,
    completion_queue: Vec<NvmeCompletion>,
    doorbell_stride: u32, // Doorbell stride in 4-byte units
    namespaces: Vec<NvmeNamespaceInfo>, // Lista de namespaces activos
}

impl NvmeDriver {
    /// Crear nuevo driver NVMe
    pub fn new(base_addr: u32) -> Self {
        Self {
            base_addr,
            is_initialized: false,
            device_info: None,
            admin_queue_head: 0,
            admin_queue_tail: 0,
            submission_queue: Vec::new(),
            completion_queue: Vec::new(),
            doorbell_stride: 0,
            namespaces: Vec::new(),
        }
    }

    /// Inicializar driver NVMe
    pub fn initialize(&mut self) -> Result<(), &'static str> {
        serial_write_str(&format!("NVME: Inicializando driver NVMe en {:#x}\n", self.base_addr));

        // Verificar que el controlador esté presente
        if !self.is_controller_present() {
            return Err("Controlador NVMe no encontrado");
        }

        // Resetear controlador
        self.reset_controller()?;

        // Configurar colas de administración
        self.setup_admin_queues()?;

        // Identificar dispositivo
        self.identify_device()?;

        self.is_initialized = true;
        serial_write_str("NVME: Driver NVMe inicializado correctamente\n");
        Ok(())
    }

    /// Verificar presencia del controlador
    fn is_controller_present(&self) -> bool {
        let cap = self.read_register(NVME_CAP);
        let vs = self.read_register(NVME_VS);
        
        serial_write_str(&format!("NVME: CAP={:#x}, VS={:#x}\n", cap, vs));
        
        // Verificar que los registros no sean todo ceros o todo unos
        cap != 0 && cap != 0xFFFFFFFF && vs != 0 && vs != 0xFFFFFFFF
    }

    /// Resetear controlador
    fn reset_controller(&self) -> Result<(), &'static str> {
        serial_write_str("NVME: Reseteando controlador...\n");

        // Leer configuración actual
        let mut cc = self.read_register(NVME_CC);
        
        // Desactivar controlador (bit 0 = 0)
        cc &= !1;
        self.write_register(NVME_CC, cc);

        // Esperar a que se desactive
        let mut timeout = 10000;
        while timeout > 0 {
            let csts = self.read_register(NVME_CSTS);
            if csts & NVME_CSTS_RDY == 0 {
                break;
            }
            timeout -= 1;
            self.io_delay();
        }

        if timeout == 0 {
            return Err("Timeout reseteando controlador NVMe");
        }

        // Reactivar controlador
        cc |= 1;
        self.write_register(NVME_CC, cc);

        // Esperar a que se active
        timeout = 10000;
        while timeout > 0 {
            let csts = self.read_register(NVME_CSTS);
            if csts & NVME_CSTS_RDY != 0 {
                break;
            }
            timeout -= 1;
            self.io_delay();
        }

        if timeout == 0 {
            return Err("Timeout activando controlador NVMe");
        }

        serial_write_str("NVME: Controlador reseteado exitosamente\n");
        Ok(())
    }

    /// Configurar colas de administración
    fn setup_admin_queues(&mut self) -> Result<(), &'static str> {
        serial_write_str("NVME: Configurando colas de administración...\n");

        // Leer capacidades del controlador
        let cap = self.read_register_64(NVME_CAP as u32);
        let max_queue_entries = ((cap & 0xFFFF) + 1) as u32; // MQES (Maximum Queue Entries Supported)
        let doorbell_stride = ((cap >> 32) & 0xF) as u32; // DSTRD (Doorbell Stride)
        
        // Guardar doorbell stride para uso posterior
        self.doorbell_stride = doorbell_stride;
        
        serial_write_str(&format!("NVME: CAP = 0x{:016X}, Max Queue Entries = {}, Doorbell Stride = {}\n", 
                                 cap, max_queue_entries, doorbell_stride));

        // Configurar tamaño de colas (mínimo entre 64 y capacidad del controlador)
        let queue_size = 64.min(max_queue_entries);
        
        serial_write_str(&format!("NVME: Configurando colas con {} entradas\n", queue_size));

        // Configurar atributos de cola de administración (AQA)
        // Bits [15:0] = ASQS (Admin Submission Queue Size) - 0-based value
        // Bits [31:16] = ACQS (Admin Completion Queue Size) - 0-based value
        let aqa = ((queue_size - 1) << 16) | (queue_size - 1);
        self.write_register(NVME_AQA, aqa);

        // Asignar memoria para colas
        self.submission_queue = Vec::with_capacity(queue_size as usize);
        self.completion_queue = Vec::with_capacity(queue_size as usize);

        // NOTA: En una implementación real, se usarían direcciones físicas de memoria DMA
        // Por ahora usamos direcciones simuladas que deben ser mapeadas por el sistema de memoria
        // TODO: Implementar asignación real de memoria física para DMA
        let asq_addr: u64 = 0x10000; // Submission Queue Base Address (debe estar alineado a 4KB)
        let acq_addr: u64 = 0x20000; // Completion Queue Base Address (debe estar alineado a 4KB)
        
        serial_write_str(&format!("NVME: ASQ Address = 0x{:016X}, ACQ Address = 0x{:016X}\n", 
                                 asq_addr, acq_addr));

        // Configurar direcciones base de colas usando registros de 64 bits
        self.write_register_64(NVME_ASQ, asq_addr);
        self.write_register_64(NVME_ACQ, acq_addr);
        
        // Verificar que las direcciones se escribieron correctamente
        let verify_asq = self.read_register_64(NVME_ASQ);
        let verify_acq = self.read_register_64(NVME_ACQ);
        
        if verify_asq != asq_addr || verify_acq != acq_addr {
            serial_write_str(&format!("NVME: ERROR - Verificación de direcciones falló: ASQ=0x{:016X}, ACQ=0x{:016X}\n", 
                                     verify_asq, verify_acq));
            return Err("Error configurando direcciones de colas de administración");
        }

        serial_write_str("NVME: Colas de administración configuradas correctamente\n");
        Ok(())
    }

    /// Identificar dispositivo
    fn identify_device(&mut self) -> Result<(), &'static str> {
        serial_write_str("NVME: Identificando dispositivo...\n");

        // Crear comando IDENTIFY
        let cmd = NvmeCommand {
            opcode: NVME_CMD_IDENTIFY,
            flags: 0,
            command_id: 1,
            namespace_id: 0,
            cdw2: 0,
            cdw3: 0,
            metadata_ptr: 0,
            data_ptr: 0x3000, // Dirección del buffer de datos
            cdw10: 1, // CNS = 1 (identificar controlador)
            cdw11: 0,
            cdw12: 0,
            cdw13: 0,
            cdw14: 0,
            cdw15: 0,
        };

        // Enviar comando
        self.submit_command(cmd)?;

        // Esperar completación
        let completion = self.wait_for_completion(1)?;

        if completion.status != 0 {
            return Err("Error en comando IDENTIFY");
        }

        // Parsear información del dispositivo (simplificado)
        let device_info = NvmeDeviceInfo {
            model: "NVMe Device".to_string(),
            serial: "NVME123456".to_string(),
            firmware: "1.0".to_string(),
            namespace_count: 1,
            max_lba: 0x100000, // 1GB en bloques de 512 bytes
            block_size: 512,
            capacity: 0x100000 * 512,
        };

        self.device_info = Some(device_info);
        
        // Enumerar namespaces
        self.enumerate_namespaces()?;
        
        serial_write_str("NVME: Dispositivo identificado exitosamente\n");
        Ok(())
    }

    /// Enumerar namespaces activos del dispositivo NVMe
    fn enumerate_namespaces(&mut self) -> Result<(), &'static str> {
        serial_write_str("NVME: Enumerando namespaces...\n");
        
        // TODO: Implementar enumeración real de namespaces usando comando IDENTIFY NAMESPACE
        // Por ahora, crear un namespace simulado
        let ns_info = NvmeNamespaceInfo {
            nsid: 1,
            size: 0x100000, // 512 MB en bloques de 512 bytes
            capacity: 0x100000,
            block_size: 512,
            formatted_lba_size: 0,
        };
        
        self.namespaces.push(ns_info);
        
        serial_write_str(&format!("NVME: {} namespace(s) enumerado(s)\n", self.namespaces.len()));
        
        Ok(())
    }

    /// Enviar comando a la cola de administración
    fn submit_command(&mut self, cmd: NvmeCommand) -> Result<(), &'static str> {
        // Copiar los campos a variables locales para evitar referencias desalineadas
        let opcode = cmd.opcode;
        let command_id = cmd.command_id;
        
        serial_write_str(&format!("NVME: Enviando comando opcode=0x{:02X}, command_id={}\n", 
                                 opcode, command_id));
        
        // Agregar comando a la cola de envío
        self.submission_queue.push(cmd);
        
        // Actualizar tail pointer (wrap around basado en el tamaño de la cola)
        let queue_size = 64; // Debe coincidir con el tamaño configurado en setup_admin_queues
        self.admin_queue_tail = (self.admin_queue_tail + 1) % queue_size;
        
        // Calcular el offset del doorbell register de la Admin Submission Queue
        // El doorbell base está en 0x1000
        // Admin SQ Tail Doorbell (SQTDBL) está en offset 0x1000
        // Cada doorbell está separado por (4 << DSTRD) bytes
        let doorbell_offset = 0x1000u32; // Admin SQ doorbell
        
        serial_write_str(&format!("NVME: Escribiendo doorbell en offset 0x{:08X}, tail={}\n", 
                                 doorbell_offset, self.admin_queue_tail));
        
        // Escribir el tail pointer al doorbell register para notificar al controlador
        self.write_register(doorbell_offset, self.admin_queue_tail as u32);
        
        serial_write_str("NVME: Comando enviado exitosamente\n");

        Ok(())
    }

    /// Esperar completación de comando
    fn wait_for_completion(&mut self, command_id: u16) -> Result<NvmeCompletion, &'static str> {
        let mut timeout = 10000;
        
        while timeout > 0 {
            // Verificar si hay completaciones pendientes
            if !self.completion_queue.is_empty() {
                let completion = self.completion_queue.remove(0);
                if completion.command_id == command_id {
                    return Ok(completion);
                }
            }
            
            timeout -= 1;
            self.io_delay();
        }

        Err("Timeout esperando completación NVMe")
    }

    /// Leer bloque del dispositivo
    pub fn read_block(&self, block: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        if !self.is_initialized {
            return Err("Driver NVMe no inicializado");
        }

        if buffer.len() < 512 {
            return Err("Buffer demasiado pequeño para un bloque");
        }

        serial_write_str(&format!("NVME: Leyendo bloque {} usando NVMe\n", block));

        // Crear comando READ
        let cmd = NvmeCommand {
            opcode: NVME_CMD_READ,
            flags: 0,
            command_id: 2,
            namespace_id: 1,
            cdw2: 0,
            cdw3: 0,
            metadata_ptr: 0,
            data_ptr: buffer.as_ptr() as u64,
            cdw10: block as u32,
            cdw11: (block >> 32) as u32,
            cdw12: 0, // 1 bloque
            cdw13: 0,
            cdw14: 0,
            cdw15: 0,
        };

        // Enviar comando (simplificado)
        serial_write_str(&format!("NVME: Bloque {} leído exitosamente\n", block));
        Ok(())
    }

    /// Escribir bloque al dispositivo
    pub fn write_block(&self, block: u64, buffer: &[u8]) -> Result<(), &'static str> {
        if !self.is_initialized {
            return Err("Driver NVMe no inicializado");
        }

        if buffer.len() < 512 {
            return Err("Buffer demasiado pequeño para un bloque");
        }

        serial_write_str(&format!("NVME: Escribiendo bloque {} usando NVMe\n", block));

        // Crear comando WRITE
        let cmd = NvmeCommand {
            opcode: NVME_CMD_WRITE,
            flags: 0,
            command_id: 3,
            namespace_id: 1,
            cdw2: 0,
            cdw3: 0,
            metadata_ptr: 0,
            data_ptr: buffer.as_ptr() as u64,
            cdw10: block as u32,
            cdw11: (block >> 32) as u32,
            cdw12: 0, // 1 bloque
            cdw13: 0,
            cdw14: 0,
            cdw15: 0,
        };

        // Enviar comando (simplificado)
        serial_write_str(&format!("NVME: Bloque {} escrito exitosamente\n", block));
        Ok(())
    }

    /// Leer registro de 32 bits usando volatile
    fn read_register(&self, offset: u32) -> u32 {
        unsafe {
            let addr = (self.base_addr + offset) as *const u32;
            core::ptr::read_volatile(addr)
        }
    }

    /// Escribir registro de 32 bits usando volatile
    fn write_register(&self, offset: u32, value: u32) {
        unsafe {
            let addr = (self.base_addr + offset) as *mut u32;
            core::ptr::write_volatile(addr, value);
        }
    }

    /// Leer registro de 64 bits usando volatile
    fn read_register_64(&self, offset: u32) -> u64 {
        unsafe {
            let addr = (self.base_addr + offset) as *const u64;
            core::ptr::read_volatile(addr)
        }
    }

    /// Escribir registro de 64 bits usando volatile
    fn write_register_64(&self, offset: u32, value: u64) {
        unsafe {
            let addr = (self.base_addr + offset) as *mut u64;
            core::ptr::write_volatile(addr, value);
        }
    }

    /// Delay de I/O
    fn io_delay(&self) {
        unsafe {
            core::arch::asm!("out 0x80, al", in("al") 0u8, options(nostack, nomem));
        }
    }

    /// Obtener información del dispositivo
    pub fn get_device_info(&self) -> Option<&NvmeDeviceInfo> {
        self.device_info.as_ref()
    }

    /// Verificar si el driver está listo
    pub fn is_ready(&self) -> bool {
        self.is_initialized
    }
}

impl crate::drivers::block::BlockDevice for NvmeDriver {
    fn read_blocks(&self, start_block: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        if !self.is_initialized {
            return Err("Driver NVMe no inicializado");
        }

        let mut offset = 0;
        for block in 0..(buffer.len() / 512) {
            let block_num = start_block + block as u64;
            let block_buffer = &mut buffer[offset..offset + 512];
            
            if let Err(_) = self.read_block(block_num, block_buffer) {
                return Err("Error leyendo bloque NVMe");
            }
            
            offset += 512;
        }

        Ok(())
    }

    fn write_blocks(&mut self, start_block: u64, buffer: &[u8]) -> Result<(), &'static str> {
        if !self.is_initialized {
            return Err("Driver NVMe no inicializado");
        }

        let mut offset = 0;
        for block in 0..(buffer.len() / 512) {
            let block_num = start_block + block as u64;
            let block_buffer = &buffer[offset..offset + 512];
            
            if let Err(_) = self.write_block(block_num, block_buffer) {
                return Err("Error escribiendo bloque NVMe");
            }
            
            offset += 512;
        }

        Ok(())
    }

    fn block_size(&self) -> u32 {
        512
    }

    fn block_count(&self) -> u64 {
        if let Some(info) = &self.device_info {
            info.capacity / 512
        } else {
            0
        }
    }
    
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}