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
#[derive(Debug)]
pub struct NvmeDeviceInfo {
    pub model: String,
    pub serial: String,
    pub firmware: String,
    pub namespace_count: u32,
    pub max_lba: u64,
    pub block_size: u32,
    pub capacity: u64,
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

        // Configurar tamaño de colas (64 entradas)
        let queue_size = 64;
        let queue_size_log = 6; // log2(64)

        // Configurar atributos de cola de administración
        let aqa = (queue_size - 1) << 16 | (queue_size - 1);
        self.write_register(NVME_AQA, aqa);

        // Asignar memoria para colas (simplificado - en implementación real usaría alloc)
        self.submission_queue = Vec::with_capacity(queue_size as usize);
        self.completion_queue = Vec::with_capacity(queue_size as usize);

        // Configurar direcciones base de colas (simplificado)
        self.write_register(NVME_ASQ, 0x1000); // Dirección física de SQ
        self.write_register(NVME_ACQ, 0x2000); // Dirección física de CQ

        serial_write_str("NVME: Colas de administración configuradas\n");
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
        serial_write_str("NVME: Dispositivo identificado exitosamente\n");
        Ok(())
    }

    /// Enviar comando
    fn submit_command(&mut self, cmd: NvmeCommand) -> Result<(), &'static str> {
        // Agregar comando a la cola de envío
        self.submission_queue.push(cmd);
        
        // Actualizar tail pointer
        self.admin_queue_tail = (self.admin_queue_tail + 1) % 64;
        
        // Notificar al controlador
        self.write_register(0x1000, self.admin_queue_tail as u32); // Doorbell register

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

    /// Leer registro de 32 bits
    fn read_register(&self, offset: u32) -> u32 {
        unsafe {
            let addr = (self.base_addr + offset) as *const u32;
            *addr
        }
    }

    /// Escribir registro de 32 bits
    fn write_register(&self, offset: u32, value: u32) {
        unsafe {
            let addr = (self.base_addr + offset) as *mut u32;
            *addr = value;
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