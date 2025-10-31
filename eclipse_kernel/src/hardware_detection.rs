//! Sistema de detección de hardware para Eclipse OS
//!
//! Implementa detección automática de hardware gráfico y otros dispositivos
//! usando PCI y otros métodos de detección.

use crate::drivers::amd_graphics::Amd2DOperation;
use crate::drivers::amd_graphics::AmdGraphicsDriver;
use crate::drivers::framebuffer::{get_framebuffer, Color};
use crate::drivers::gpu_manager::{create_gpu_driver_manager, GpuDriverManager};
use crate::drivers::intel_graphics::Intel2DOperation;
use crate::drivers::intel_graphics::IntelGraphicsDriver;
use crate::drivers::nvidia_graphics::Nvidia2DOperation;
use crate::drivers::nvidia_graphics::NvidiaGraphicsDriver;
use crate::drivers::pci::{GpuInfo, GpuType, PciManager};
use crate::drivers::pci_polished::PolishedPciDriver;
use crate::debug::serial_write_str;
use crate::syslog_info;
use crate::uefi_framebuffer::{get_framebuffer_status, is_framebuffer_initialized};
use alloc::format;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec;
use alloc::vec::Vec;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicBool, Ordering};

/// Resultado de la detección de hardware
#[derive(Debug, Clone)]
pub struct HardwareDetectionResult {
    pub graphics_mode: GraphicsMode,
    pub primary_gpu: Option<GpuInfo>,
    pub available_gpus: Vec<GpuInfo>,
    pub framebuffer_available: bool,
    pub vga_available: bool,
    pub recommended_driver: RecommendedDriver,
    pub gpu_driver_manager: Option<GpuDriverManager>,
    pub nvme_controller_available: bool, // Nuevo campo
    pub sata_controller_available: bool, // Nuevo campo
    pub pci_manager: PciManager, // Nuevo campo
    pub polished_pci_driver: Option<PolishedPciDriver>, // Driver PCI robusto
}

/// Modos de gráficos disponibles
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GraphicsMode {
    Framebuffer,
    VGA,
    HardwareAccelerated,
}

/// Drivers recomendados
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RecommendedDriver {
    Intel,
    Nvidia,
    Amd,
    GenericFramebuffer,
    VGA,
    Unknown,
}

impl RecommendedDriver {
    pub fn as_str(&self) -> &'static str {
        match self {
            RecommendedDriver::Intel => "Intel Graphics Driver",
            RecommendedDriver::Nvidia => "NVIDIA Driver",
            RecommendedDriver::Amd => "AMD Radeon Driver",
            RecommendedDriver::GenericFramebuffer => "Generic Framebuffer Driver",
            RecommendedDriver::VGA => "VGA Text Mode Driver",
            RecommendedDriver::Unknown => "Unknown Driver",
        }
    }
}

/// Gestor de detección de hardware
pub struct HardwareDetector {
    pci_manager: PciManager,
    detection_result: Option<HardwareDetectionResult>,
}

impl HardwareDetector {
    pub fn new() -> Self {
        Self {
            pci_manager: PciManager::new(),
            detection_result: None,
        }
    }

    /// Realizar detección completa de hardware
    pub fn detect_hardware(&mut self) -> &HardwareDetectionResult {
        // Escanear dispositivos PCI reales del sistema
        serial_write_str("HARDWARE_DETECTION: Escaneando dispositivos PCI con PciManager...\n");
        self.pci_manager.scan_devices();
        serial_write_str(&format!("HARDWARE_DETECTION: PciManager escaneó {} dispositivos totales, {} guardados\n", 
                                 self.pci_manager.total_device_count(), self.pci_manager.device_count()));
        
        // Mostrar en pantalla para hardware real
        if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
            let _ = fb.write_text_kernel(&format!("HARDWARE_DETECTION: PciManager - {} total, {} guardados", 
                                              self.pci_manager.total_device_count(), self.pci_manager.device_count()), Color::YELLOW);
        }

        // Detectar controladoras de almacenamiento
        let mut nvme_found = false;
        let mut sata_found = false;
        serial_write_str("HARDWARE_DETECTION: Analizando dispositivos PCI detectados...\n");
        for i in 0..self.pci_manager.device_count() {
            if let Some(dev) = self.pci_manager.get_device(i) {
                serial_write_str(&format!("HARDWARE_DETECTION: PCI Device {} - VID:{:04X} DID:{:04X} Class:{:02X}.{:02X}.{:02X}\n", 
                                         i, dev.vendor_id, dev.device_id, dev.class_code, dev.subclass_code, dev.prog_if));
                
                // Listado de dispositivos PCI removido para limpiar pantalla
                
                if dev.class_code == 0x01 { // Mass Storage Controller
                    match dev.subclass_code {
                        0x08 => {
                            nvme_found = true; // NVM Express
                            serial_write_str(&format!("HARDWARE_DETECTION: NVMe encontrado - VID:{:04X} DID:{:04X}\n", dev.vendor_id, dev.device_id));
                            if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                                let _ = fb.write_text_kernel(&format!("NVMe: {:04X}:{:04X}", dev.vendor_id, dev.device_id), Color::BLUE);
                            }
                        }
                        0x06 => {
                            sata_found = true; // Serial ATA (AHCI)
                            serial_write_str(&format!("HARDWARE_DETECTION: AHCI/SATA encontrado - VID:{:04X} DID:{:04X}\n", dev.vendor_id, dev.device_id));
                            if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                                let _ = fb.write_text_kernel(&format!("AHCI/SATA: {:04X}:{:04X}", dev.vendor_id, dev.device_id), Color::GREEN);
                            }
                        }
                        0x04 => {
                            // RAID Controllers - incluye SATA en modo RAID
                            // Detectar específicamente controladoras SATA Intel en modo RAID
                            let is_sata_raid = dev.vendor_id == 0x8086 && matches!(dev.device_id, 
                                0x2822 | 0x2826 | 0x282A | 0x282E | 0x282F | 0x2922 | 0x2926 | 0x292A | 0x292E | 0x292F);
                            
                            if is_sata_raid {
                                sata_found = true; // SATA en modo RAID
                                serial_write_str(&format!("HARDWARE_DETECTION: SATA RAID encontrado - VID:{:04X} DID:{:04X}\n", dev.vendor_id, dev.device_id));
                                if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                                    let _ = fb.write_text_kernel(&format!("SATA RAID: {:04X}:{:04X}", dev.vendor_id, dev.device_id), Color::GREEN);
                                }
                            } else {
                                // RAID genérico
                                serial_write_str(&format!("HARDWARE_DETECTION: RAID genérico encontrado - VID:{:04X} DID:{:04X}\n", dev.vendor_id, dev.device_id));
                                if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                                    let _ = fb.write_text_kernel(&format!("RAID: {:04X}:{:04X}", dev.vendor_id, dev.device_id), Color::YELLOW);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        // Obtener GPUs reales detectadas del bus PCI
        let mut gpus: Vec<GpuInfo> = self
            .pci_manager
            .get_gpus()
            .iter()
            .filter_map(|gpu| gpu.clone())
            .collect();
            
        // Debug: mostrar información detallada de cada GPU detectada
        for (i, gpu) in gpus.iter().enumerate() {
            serial_write_str(&format!("HARDWARE_DETECTION: GPU {} - VID:{:04X} DID:{:04X} Class:{:02X} Subclass:{:02X} Type:{:?}\n", 
                                     i, gpu.pci_device.vendor_id, gpu.pci_device.device_id, 
                                     gpu.pci_device.class_code, gpu.pci_device.subclass_code, gpu.gpu_type));
            
            if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                let _ = fb.write_text_kernel(&format!("GPU {}: {:04X}:{:04X} Class:{:02X}.{:02X} {:?}", 
                                                      i, gpu.pci_device.vendor_id, gpu.pci_device.device_id, 
                                                      gpu.pci_device.class_code, gpu.pci_device.subclass_code, gpu.gpu_type), Color::MAGENTA);
            }
        }
            
        // Debug: mostrar cuántas GPUs detectó PciManager
        serial_write_str(&format!("HARDWARE_DETECTION: PciManager detectó {} GPUs\n", gpus.len()));
        
        // Mostrar en pantalla para hardware real
        if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
            let _ = fb.write_text_kernel(&format!("HARDWARE_DETECTION: {} GPUs detectadas por PciManager", gpus.len()), Color::CYAN);
        }

        // Si no se detectaron GPUs PCI, omitir VGA legacy temporalmente
        if gpus.is_empty() {
            // omitido
        }

        // Obtener GPU primaria real
        // Seleccionar GPU primaria: priorizar GPUs reales sobre virtuales (QEMU/Bochs, Virtio, VMware)
        let mut primary_gpu = self.pci_manager.get_primary_gpu().cloned();
        // Si no hay primaria marcada o es virtual, intentamos elegir una real
        let is_virtual =
            |t: GpuType| matches!(t, GpuType::QemuBochs | GpuType::Virtio | GpuType::Vmware);
        if primary_gpu.map_or(true, |g| is_virtual(g.gpu_type)) {
            if let Some(idx) = gpus.iter().position(|g| !is_virtual(g.gpu_type)) {
                primary_gpu = gpus.get(idx).cloned();
            } else {
                // No hay reales; usar la primera virtual si existe
                primary_gpu = gpus.first().cloned();
            }
        }

        // Verificar disponibilidad real de framebuffer
        let framebuffer_available = is_framebuffer_initialized();

        // Verificar VGA legacy real (omitido temporalmente para evitar I/O a puertos)
        let vga_available = false;

        // Determinar modo de gráficos basado en hardware real
        let graphics_mode = self.determine_graphics_mode(&gpus, framebuffer_available);

        // Determinar driver recomendado basado en hardware real
        let recommended_driver =
            self.determine_recommended_driver(&primary_gpu, framebuffer_available);

        // Cargar drivers reales para GPUs detectadas
        let mut gpu_driver_manager = create_gpu_driver_manager();

        let fb_info = get_framebuffer().map(|fb| *fb.get_info());
        let fb_info_ref = fb_info.as_ref();

        if !gpus.is_empty() {
            // Log detallado de inicialización de drivers GPU
            serial_write_str(&format!("HARDWARE_DETECTION: Inicializando {} drivers GPU detectados\n", gpus.len()));
            if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                let _ = fb.write_text_kernel(&format!("GPU: Inicializando {} drivers detectados", gpus.len()), Color::MAGENTA);
            }
            
            match gpu_driver_manager.load_drivers_for_gpus(&gpus, fb_info_ref) {
                Ok(_) => {
                    serial_write_str("HARDWARE_DETECTION: Drivers GPU cargados exitosamente\n");
                    if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                        let _ = fb.write_text_kernel("GPU: Drivers cargados exitosamente", Color::GREEN);
                    }
                    
                    if let Err(e) = gpu_driver_manager.initialize_all_drivers() {
                        syslog_info!("GPU", &format!("Error inicializando drivers: {}", e));
                        serial_write_str(&format!("HARDWARE_DETECTION: Error inicializando drivers GPU: {}\n", e));
                        if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                            let _ = fb.write_text_kernel(&format!("GPU: Error inicialización: {}", e), Color::RED);
                        }
                    } else {
                        serial_write_str("HARDWARE_DETECTION: Drivers GPU inicializados exitosamente\n");
                        if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                            let _ = fb.write_text_kernel("GPU: Drivers inicializados exitosamente", Color::GREEN);
                        }
                        
                        // Verificar configuración de GPU dual
                        if gpu_driver_manager.is_dual_gpu_active() {
                            serial_write_str("HARDWARE_DETECTION: GPU dual NVIDIA detectada y configurada\n");
                            if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                                let _ = fb.write_text_kernel("GPU: Configuración dual activa", Color::MAGENTA);
                            }
                            
                            // Mostrar información de GPU dual
                            if let Some((gpu1_mem, gpu2_mem, total_mem)) = gpu_driver_manager.get_dual_gpu_info() {
                                serial_write_str(&format!("HARDWARE_DETECTION: GPU 1 Memory: 0x{:016X}, GPU 2 Memory: 0x{:016X}, Total: 0x{:016X}\n", 
                                                         gpu1_mem, gpu2_mem, total_mem));
                                if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                                    let _ = fb.write_text_kernel(&format!("GPU: Memoria total 0x{:016X}", total_mem), Color::CYAN);
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    syslog_info!("GPU", &format!("Error cargando drivers: {}", e));
                    serial_write_str(&format!("HARDWARE_DETECTION: Error cargando drivers GPU: {}\n", e));
                    if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                        let _ = fb.write_text_kernel(&format!("GPU: Error carga: {}", e), Color::RED);
                    }
                }
            }
        } else {
            serial_write_str("HARDWARE_DETECTION: No hay GPUs para inicializar drivers\n");
            if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                let _ = fb.write_text_kernel("GPU: No hay GPUs detectadas", Color::YELLOW);
            }
        }

        // Crear resultado con hardware real detectado
        // Inicializar driver PCI polished para hardware real
        serial_write_str("HARDWARE_DETECTION: Creando PolishedPciDriver...\n");
        if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
            let _ = fb.write_text_kernel("HARDWARE_DETECTION: Creando PolishedPciDriver...", Color::CYAN);
        }
        
        let mut polished_pci = PolishedPciDriver::new();
        serial_write_str("HARDWARE_DETECTION: Llamando polished_pci.initialize()...\n");
        if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
            let _ = fb.write_text_kernel("HARDWARE_DETECTION: Inicializando polished_pci...", Color::CYAN);
        }
        
        let polished_pci_result = polished_pci.initialize();
        
        // En hardware real, polished_pci debería detectar dispositivos correctamente
        let polished_pci_success = match polished_pci_result {
            Ok(_) => {
                serial_write_str(&format!("HARDWARE_DETECTION: Polished PCI detectó {} dispositivos en hardware real\n", 
                                         polished_pci.get_device_count()));
                if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                    let _ = fb.write_text_kernel(&format!("HARDWARE_DETECTION: Polished PCI - {} dispositivos", 
                                                      polished_pci.get_device_count()), Color::GREEN);
                }
                true
            }
            Err(e) => {
                serial_write_str(&format!("HARDWARE_DETECTION: Polished PCI falló: {}\n", e));
                if let Some(fb) = crate::drivers::framebuffer::get_framebuffer() {
                    let _ = fb.write_text_kernel(&format!("HARDWARE_DETECTION: Polished PCI falló: {}", e), Color::RED);
                }
                false
            }
        };
        
        let result = HardwareDetectionResult {
            graphics_mode,
            primary_gpu,
            available_gpus: gpus,
            framebuffer_available,
            vga_available,
            recommended_driver,
            gpu_driver_manager: Some(gpu_driver_manager),
            nvme_controller_available: nvme_found, // Asignar resultado
            sata_controller_available: sata_found, // Asignar resultado
            pci_manager: self.pci_manager.clone(), // Clonar el gestor
            polished_pci_driver: if polished_pci_success { Some(polished_pci) } else { None },
        };

        self.detection_result = Some(result);
        self.detection_result.as_ref().unwrap()
    }

    /// Determinar el mejor modo de gráficos
    fn determine_graphics_mode(
        &self,
        gpus: &[GpuInfo],
        framebuffer_available: bool,
    ) -> GraphicsMode {
        // Si hay GPU con aceleración 3D, usar hardware acelerado
        if gpus.iter().any(|gpu| gpu.supports_3d) {
            return GraphicsMode::HardwareAccelerated;
        }

        // Si hay framebuffer disponible, usarlo
        if framebuffer_available {
            return GraphicsMode::Framebuffer;
        }

        // Si hay GPU con aceleración 2D, usar hardware acelerado
        if gpus.iter().any(|gpu| gpu.supports_2d) {
            return GraphicsMode::HardwareAccelerated;
        }

        // Fallback a VGA
        GraphicsMode::VGA
    }

    /// Determinar el driver recomendado
    fn determine_recommended_driver(
        &self,
        primary_gpu: &Option<GpuInfo>,
        framebuffer_available: bool,
    ) -> RecommendedDriver {
        if let Some(gpu) = primary_gpu {
            match gpu.gpu_type {
                GpuType::Intel => RecommendedDriver::Intel,
                GpuType::Nvidia => RecommendedDriver::Nvidia,
                GpuType::Amd => RecommendedDriver::Amd,
                _ => {
                    if framebuffer_available {
                        RecommendedDriver::GenericFramebuffer
                    } else {
                        RecommendedDriver::VGA
                    }
                }
            }
        } else if framebuffer_available {
            RecommendedDriver::GenericFramebuffer
        } else {
            RecommendedDriver::VGA
        }
    }

    /// Obtener información detallada del framebuffer
    pub fn get_framebuffer_info(&self) -> Option<String> {
        if !is_framebuffer_initialized() {
            return None;
        }

        let status = get_framebuffer_status();
        if let Some(info) = status.driver_info {
            Some(format!(
                "Framebuffer: {}x{} @ {} bytes, Format: {:?}",
                info.width, info.height, info.base_address, info.pixel_format
            ))
        } else {
            Some("Framebuffer: Información no disponible".to_string())
        }
    }

    /// Obtener información de GPUs detectadas
    pub fn get_gpu_info(&self) -> Vec<String> {
        let mut info = Vec::new();

        for (i, gpu) in self.pci_manager.get_gpus().iter().enumerate() {
            if let Some(gpu) = gpu {
                info.push(format!(
                    "GPU {}: {} {} (Bus: {:02X}:{:02X}.{}) - {}MB, 2D: {}, 3D: {}, Max: {}x{}",
                    i + 1,
                    gpu.gpu_type.as_str(),
                    format!(
                        "{:04X}:{:04X}",
                        gpu.pci_device.vendor_id, gpu.pci_device.device_id
                    ),
                    gpu.pci_device.bus,
                    gpu.pci_device.device,
                    gpu.pci_device.function,
                    gpu.memory_size / (1024 * 1024),
                    if gpu.supports_2d { "Sí" } else { "No" },
                    if gpu.supports_3d { "Sí" } else { "No" },
                    gpu.max_resolution.0,
                    gpu.max_resolution.1
                ));
            }
        }

        if info.is_empty() {
            info.push("No se detectaron GPUs".to_string());
        }

        info
    }

    /// Obtener información de dispositivos PCI
    pub fn get_pci_info(&self) -> Vec<String> {
        let mut info = Vec::new();

        info.push(format!(
            "Dispositivos PCI detectados: {}",
            self.pci_manager.device_count()
        ));
        info.push(format!("GPUs detectadas: {}", self.pci_manager.gpu_count()));

        // Mostrar algunos dispositivos importantes
        for i in 0..core::cmp::min(10, self.pci_manager.device_count()) {
            if let Some(device) = self.pci_manager.get_device(i) {
                info.push(format!(
                    "  {:02X}:{:02X}.{} - {:04X}:{:04X} - Class: {:02X}:{:02X}:{:02X}",
                    device.bus,
                    device.device,
                    device.function,
                    device.vendor_id,
                    device.device_id,
                    device.class_code,
                    device.subclass_code,
                    device.prog_if
                ));
            }
        }

        info
    }

    /// Obtener resultado de detección
    pub fn get_detection_result(&self) -> Option<&HardwareDetectionResult> {
        self.detection_result.as_ref()
    }

    /// Obtener resultado de detección mutable
    pub fn get_detection_result_mut(&mut self) -> Option<&mut HardwareDetectionResult> {
        self.detection_result.as_mut()
    }

    /// Obtener información de drivers de GPU cargados
    pub fn get_gpu_driver_info(&self) -> Vec<String> {
        if let Some(result) = &self.detection_result {
            if let Some(manager) = &result.gpu_driver_manager {
                return manager.get_driver_info();
            }
        }
        vec!["No hay gestor de drivers disponible".to_string()]
    }

    /// Obtener estadísticas de drivers
    pub fn get_driver_stats(&self) -> (usize, usize, usize) {
        if let Some(result) = &self.detection_result {
            if let Some(manager) = &result.gpu_driver_manager {
                return manager.get_driver_stats();
            }
        }
        (0, 0, 0)
    }

    /// Detectar VGA legacy real
    fn detect_legacy_vga(&self) -> Option<GpuInfo> {
        // Verificar si hay VGA legacy disponible
        if self.check_vga_availability() {
            Some(GpuInfo {
                pci_device: crate::drivers::pci::PciDevice {
                    bus: 0,
                    device: 2,
                    function: 0,
                    vendor_id: 0x1234, // VGA legacy
                    device_id: 0x1111,
                    class_code: 0x03,
                    subclass_code: 0x00,
                    prog_if: 0x00,
                    revision_id: 0x00,
                    header_type: 0x00,
                    status: 0x0000,
                    command: 0x0000,
                },
                gpu_type: GpuType::Unknown,
                memory_size: 0, // VGA legacy no tiene memoria dedicada
                is_primary: true,
                supports_2d: true,
                supports_3d: false,
                max_resolution: (640, 480),
            })
        } else {
            None
        }
    }

    /// Verificar disponibilidad real de VGA
    fn check_vga_availability(&self) -> bool {
        // Verificar si VGA está disponible en el sistema
        unsafe {
            // Intentar leer desde puerto VGA
            let vga_port: u16 = 0x3C0;
            let _ = core::arch::asm!(
                "in al, dx",
                in("dx") vga_port,
                out("al") _,
                options(nomem, nostack, preserves_flags)
            );
            true // Si no hay excepción, VGA está disponible
        }
    }
}

/// Función de conveniencia para detección rápida
pub fn detect_graphics_hardware() -> &'static HardwareDetectionResult {
    ensure_detector_initialized()
        .get_detection_result()
        .expect("La detección de hardware no produjo resultado")
}

/// Función de conveniencia para obtener modo de gráficos
pub fn get_graphics_mode() -> GraphicsMode {
    let result = detect_graphics_hardware();
    result.graphics_mode
}

/// Ejecutar una operación con el gestor de drivers de GPU global
pub fn with_gpu_driver_manager<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&mut GpuDriverManager) -> R,
{
    unsafe {
        let detector = detector_mut_ref();
        if detector.get_detection_result().is_none() {
            detector.detect_hardware();
        }
        let result = detector.get_detection_result_mut()?;
        let manager = result.gpu_driver_manager.as_mut()?;
        Some(f(manager))
    }
}

// --- Funciones auxiliares internas ---

static DETECTOR_INIT: AtomicBool = AtomicBool::new(false);
static mut DETECTOR: MaybeUninit<HardwareDetector> = MaybeUninit::uninit();

// SAFETY: acceso único controlado por AtomicBool y ejecutado en un único hilo
unsafe fn detector_mut_ref() -> &'static mut HardwareDetector {
    if !DETECTOR_INIT.load(Ordering::Acquire) {
        DETECTOR.write(HardwareDetector::new());
        DETECTOR_INIT.store(true, Ordering::Release);
    }
    DETECTOR.assume_init_mut()
}

fn ensure_detector_initialized() -> &'static HardwareDetector {
    unsafe {
        let detector = detector_mut_ref();
        if detector.get_detection_result().is_none() {
            detector.detect_hardware();
        }
        &*detector
    }
}
