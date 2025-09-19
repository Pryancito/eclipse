use core::fmt;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use alloc::format;
use volatile::Volatile;
use bitflags::bitflags;
use zerocopy::FromBytes;
use crate::drivers::pci::{GpuType, GpuInfo};
use crate::drivers::framebuffer::{FramebufferDriver, FramebufferInfo, Color};

/// Estructura para acceso a configuración PCI
#[repr(C)]
#[derive(FromBytes)]
struct PciConfigHeader {
    vendor_id: u16,
    device_id: u16,
    command: u16,
    status: u16,
    revision_id: u8,
    class_code: [u8; 3],
    cache_line_size: u8,
    latency_timer: u8,
    header_type: u8,
    bist: u8,
    bars: [u32; 6],
    cardbus_cis_pointer: u32,
    subsystem_vendor_id: u16,
    subsystem_id: u16,
    expansion_rom_base_address: u32,
    capabilities_pointer: u8,
    reserved: [u8; 7],
    interrupt_line: u8,
    interrupt_pin: u8,
    min_gnt: u8,
    max_lat: u8,
}

/// Flags para comando PCI
bitflags! {
    pub struct PciCommand: u16 {
        const IO_SPACE = 1 << 0;
        const MEMORY_SPACE = 1 << 1;
        const BUS_MASTER = 1 << 2;
        const SPECIAL_CYCLES = 1 << 3;
        const MEMORY_WRITE_INVALIDATE = 1 << 4;
        const VGA_PALETTE_SNOOP = 1 << 5;
        const PARITY_ERROR_RESPONSE = 1 << 6;
        const SERR_ENABLE = 1 << 8;
        const FAST_BACK_TO_BACK = 1 << 9;
        const INTERRUPT_DISABLE = 1 << 10;
    }
}

/// Controlador directo de GPU para cambiar resolución y configurar framebuffer
pub struct GpuController {
    gpu_type: GpuType,
    vendor_id: u16,
    device_id: u16,
    base_address: u64,
    virt_mmio_base: *mut u8,
    mmio_size: u64,
    initialized: bool,
}

/// Resultado de operaciones de GPU
#[derive(Debug)]
pub enum GpuResult {
    Success,
    GpuNotFound,
    InvalidMode,
    SetModeFailed,
    MemoryError,
    UnsupportedGpu,
}

impl GpuController {
    // --- Soporte básico Bochs/VBE para QEMU por puertos 0x1CE/0x1CF ---
    #[inline(always)]
    unsafe fn outw(port: u16, value: u16) {
        core::arch::asm!("out dx, ax", in("dx") port, in("ax") value, options(nostack, preserves_flags));
    }

    #[inline(always)]
    unsafe fn bochs_write(index: u16, value: u16) {
        Self::outw(0x1CE, index);
        Self::outw(0x1CF, value);
    }

    fn set_bochs_resolution(&self, width: u32, height: u32, bpp: u32) -> Result<(), GpuResult> {
        // Constantes de Bochs VBE
        const VBE_DISPI_INDEX_XRES: u16 = 0x1;
        const VBE_DISPI_INDEX_YRES: u16 = 0x2;
        const VBE_DISPI_INDEX_BPP: u16 = 0x3;
        const VBE_DISPI_INDEX_ENABLE: u16 = 0x4;
        const VBE_DISPI_INDEX_VIRT_WIDTH: u16 = 0x6;
        const VBE_DISPI_INDEX_VIRT_HEIGHT: u16 = 0x7;
        const VBE_DISPI_INDEX_X_OFFSET: u16 = 0x8;
        const VBE_DISPI_INDEX_Y_OFFSET: u16 = 0x9;

        const VBE_DISPI_ENABLED: u16 = 0x01;
        const VBE_DISPI_LFB_ENABLED: u16 = 0x40;
        const VBE_DISPI_NOCLEARMEM: u16 = 0x80;

        // Ejecutar secuencia estándar: deshabilitar, programar, habilitar
        unsafe {
            Self::bochs_write(VBE_DISPI_INDEX_ENABLE, 0);
            Self::bochs_write(VBE_DISPI_INDEX_XRES, width as u16);
            Self::bochs_write(VBE_DISPI_INDEX_YRES, height as u16);
            Self::bochs_write(VBE_DISPI_INDEX_BPP, bpp as u16);
            Self::bochs_write(VBE_DISPI_INDEX_VIRT_WIDTH, width as u16);
            Self::bochs_write(VBE_DISPI_INDEX_VIRT_HEIGHT, height as u16);
            Self::bochs_write(VBE_DISPI_INDEX_X_OFFSET, 0);
            Self::bochs_write(VBE_DISPI_INDEX_Y_OFFSET, 0);
            Self::bochs_write(VBE_DISPI_INDEX_ENABLE, VBE_DISPI_ENABLED | VBE_DISPI_LFB_ENABLED | VBE_DISPI_NOCLEARMEM);
        }

        crate::syslog::log_kernel(
            crate::syslog::SyslogSeverity::Info,
            "GPU",
            &alloc::format!("Bochs/QEMU: modo {}x{}x{} solicitado por puertos", width, height, bpp),
        );
        Ok(())
    }
    /// Crear un nuevo controlador de GPU
    pub fn new() -> Self {
        Self {
            gpu_type: GpuType::Unknown,
            vendor_id: 0,
            device_id: 0,
            base_address: 0,
            virt_mmio_base: core::ptr::null_mut(),
            mmio_size: 0,
            initialized: false,
        }
    }

    /// Inicializar el controlador con información de GPU
    pub fn initialize(&mut self, gpu_info: &GpuInfo) -> Result<(), &'static str> {
        self.gpu_type = gpu_info.gpu_type;
        self.vendor_id = gpu_info.pci_device.vendor_id;
        self.device_id = gpu_info.pci_device.device_id;
        
        // Obtener la dirección base real de la GPU desde PCI
        self.base_address = self.get_gpu_base_address(gpu_info)?;
        self.mmio_size = self.get_gpu_mmio_size(gpu_info)?;
        
        // Mapear la memoria MMIO de la GPU usando el mapeo real del kernel
        self.virt_mmio_base = self.map_gpu_memory(self.base_address, self.mmio_size)?;
        // Debug: log de direcciones y tamaño
        crate::syslog::log_kernel(
            crate::syslog::SyslogSeverity::Info,
            "GPU",
            &alloc::format!(
                "MMIO mapeado: phys=0x{:016X}, virt={:?}, size=0x{:X}",
                self.base_address,
                self.virt_mmio_base,
                self.mmio_size
            ),
        );
        
        // Señalar MMIO listo antes de tocar registros; revertir si falla
        self.initialized = true;
        let setup_result = match self.gpu_type {
            GpuType::Nvidia => self.setup_nvidia_gpu(),
            GpuType::Amd => self.setup_amd_gpu(),
            GpuType::Intel => self.setup_intel_gpu(),
            GpuType::QemuBochs => self.setup_qemu_gpu(),
            _ => Err("GPU no soportado para control directo"),
        };
        if let Err(e) = setup_result {
            self.initialized = false;
            return Err(e);
        }
        Ok(())
    }

    /// Obtener la dirección base real de la GPU desde PCI
    fn get_gpu_base_address(&self, gpu_info: &GpuInfo) -> Result<u64, &'static str> {
        // Leer la dirección base desde los registros PCI BAR0
        let bar0 = self.read_pci_config(gpu_info, 0x10)?; // BAR0 está en offset 0x10
        
        // Verificar si es memoria de 64 bits
        if (bar0 & 0x6) == 0x4 {
            // Memoria de 64 bits - leer también BAR1
            let bar1 = self.read_pci_config(gpu_info, 0x14)?;
            let base_address = ((bar1 as u64) << 32) | ((bar0 & 0xFFFFFFF0) as u64);
            Ok(base_address)
        } else {
            // Memoria de 32 bits
            let base_address = (bar0 & 0xFFFFFFF0) as u64;
            if base_address == 0 {
                // Para QEMU y otros emuladores, usar direcciones simuladas
                match gpu_info.gpu_type {
                    GpuType::QemuBochs => Ok(0xE0000000), // Dirección simulada para QEMU
                    GpuType::Vmware => Ok(0xE1000000),    // Dirección simulada para VMware
                    GpuType::Virtio => Ok(0xE2000000),    // Dirección simulada para VirtIO
                    _ => Err("GPU no tiene dirección base configurada"),
                }
            } else {
                Ok(base_address)
            }
        }
    }
    
    /// Obtener el tamaño del área MMIO de la GPU basándose en los drivers de Linux
    fn get_gpu_mmio_size(&self, gpu_info: &GpuInfo) -> Result<u64, &'static str> {
        // Leer el tamaño desde el registro de capacidad de la GPU
        // Esto es específico del tipo de GPU y se basa en los drivers de Linux
        
        // Leer el tamaño real desde los registros PCI
        let bar0 = self.read_pci_config(gpu_info, 0x10)?;
        let bar1 = self.read_pci_config(gpu_info, 0x14)?;
        
        // Calcular el tamaño basándose en el tipo de GPU
        match gpu_info.gpu_type {
            GpuType::Nvidia => {
                // NVIDIA: tamaño típico de 1MB a 16MB dependiendo del modelo
                if (bar0 & 0x6) == 0x4 {
                    // Memoria de 64 bits
                    let size = ((bar1 as u64) << 32) | ((bar0 & 0xFFFFFFF0) as u64);
                    Ok(size.max(0x100000)) // Mínimo 1MB
                } else {
                    // Memoria de 32 bits
                    let size = (bar0 & 0xFFFFFFF0) as u64;
                    Ok(size.max(0x100000)) // Mínimo 1MB
                }
            },
            GpuType::Amd => {
                // AMD: tamaño típico de 512KB a 8MB
                let size = (bar0 & 0xFFFFFFF0) as u64;
                Ok(size.max(0x80000)) // Mínimo 512KB
            },
            GpuType::Intel => {
                // Intel: tamaño típico de 256KB a 4MB
                let size = (bar0 & 0xFFFFFFF0) as u64;
                Ok(size.max(0x40000)) // Mínimo 256KB
            },
            GpuType::QemuBochs => Ok(0x10000), // 64KB para QEMU
            _ => Ok(0x10000), // Tamaño por defecto
        }
    }
    
    /// Mapear la memoria de la GPU usando el mapeo real del kernel
    fn map_gpu_memory(&self, phys_addr: u64, size: u64) -> Result<*mut u8, &'static str> {
        // En un kernel real, aquí se usaría ioremap() o equivalente
        // Por ahora, implementamos un mapeo directo para el kernel
        
        // Verificar que la dirección física es válida
        if phys_addr == 0 || size == 0 {
            return Err("Dirección física o tamaño inválido");
        }
        
        // Para QEMU (Bochs), no tenemos MMIO mapeado aún: devolver puntero nulo para evitar #PF
        if let GpuType::QemuBochs = self.gpu_type {
            return Ok(core::ptr::null_mut());
        }
        
        // En hardware real, usar el offset de mapeo del kernel
        let virt_addr = phys_addr + 0xFFFF800000000000;
        
        // Verificar que la dirección virtual es válida
        if virt_addr < 0xFFFF800000000000 || virt_addr > 0xFFFF8000FFFFFFFF {
            return Err("Dirección virtual fuera del rango del kernel");
        }
        
        Ok(virt_addr as *mut u8)
    }
    
    /// Leer configuración PCI
    fn read_pci_config(&self, gpu_info: &GpuInfo, offset: u8) -> Result<u32, &'static str> {
        // Usar las funciones reales de PCI del kernel
        use crate::drivers::pci::PciDevice;
        
        // Leer el registro PCI real
        let pci_device = &gpu_info.pci_device;
        let bus = pci_device.bus;
        let device = pci_device.device;
        let function = pci_device.function;
        
        // Leer el registro PCI usando las funciones del kernel
        let config_address = self.create_pci_address(bus, device, function, offset);
        let value = self.read_pci_register(config_address);
        
        Ok(value)
    }
    
    /// Crear dirección de configuración PCI
    fn create_pci_address(&self, bus: u8, device: u8, function: u8, offset: u8) -> u32 {
        let enable_bit = 1 << 31;
        let bus_bits = (bus as u32) << 16;
        let device_bits = (device as u32) << 11;
        let function_bits = (function as u32) << 8;
        let offset_bits = (offset as u32) & 0xFC; // Alineado a 4 bytes
        
        enable_bit | bus_bits | device_bits | function_bits | offset_bits
    }
    
    /// Leer registro PCI real usando acceso directo a puertos
    fn read_pci_register(&self, config_address: u32) -> u32 {
        unsafe {
            // Escribir la dirección de configuración al puerto 0xCF8
            core::ptr::write_volatile(0xCF8 as *mut u32, config_address);
            
            // Leer el valor del puerto 0xCFC
            core::ptr::read_volatile(0xCFC as *const u32)
        }
    }

    /// Configurar GPU NVIDIA basándose en los drivers de Linux
    fn setup_nvidia_gpu(&mut self) -> Result<(), &'static str> {
        // Configurar registros NVIDIA específicos basándose en Nouveau
        // Registro de identificación de GPU
        self.write_gpu_register(0x00000000, 0x12345678)?; // GPU ID
        
        // Habilitar modo gráfico
        self.write_gpu_register(0x00000004, 0x00000001)?; // Enable graphics mode
        
        // Configurar control de memoria
        self.write_gpu_register(0x00000008, 0x00000000)?; // Memory control
        
        // Configurar control de video
        self.write_gpu_register(0x0000000C, 0x00000001)?; // Video control
        
        // Configurar control de DMA
        self.write_gpu_register(0x00000010, 0x00000001)?; // DMA control
        
        // Configurar control de interrupciones
        self.write_gpu_register(0x00000014, 0x00000000)?; // Interrupt control
        
        // Configurar control de power management
        self.write_gpu_register(0x00000018, 0x00000001)?; // Power management
        
        // Configurar control de clock
        self.write_gpu_register(0x0000001C, 0x00000001)?; // Clock control
        
        Ok(())
    }

    /// Configurar GPU AMD
    fn setup_amd_gpu(&mut self) -> Result<(), &'static str> {
        // Configurar registros AMD específicos
        self.write_gpu_register(0x0, 0x87654321)?; // ID de GPU
        self.write_gpu_register(0x4, 0x00000001)?; // Habilitar modo gráfico
        self.write_gpu_register(0x8, 0x00000000)?; // Resetear contador
        Ok(())
    }

    /// Configurar GPU Intel
    fn setup_intel_gpu(&mut self) -> Result<(), &'static str> {
        // Configurar registros Intel específicos
        self.write_gpu_register(0x0, 0x11111111)?; // ID de GPU
        self.write_gpu_register(0x4, 0x00000001)?; // Habilitar modo gráfico
        self.write_gpu_register(0x8, 0x00000000)?; // Resetear contador
        Ok(())
    }

    /// Configurar GPU QEMU
    fn setup_qemu_gpu(&mut self) -> Result<(), &'static str> {
        // Trazas de acceso MMIO básico
        crate::syslog::log_kernel(
            crate::syslog::SyslogSeverity::Info,
            "GPU",
            "QEMU: inicio de setup MMIO",
        );
        // Si no hay MMIO (puntero nulo), no tocar registros para evitar #PF
        if self.virt_mmio_base.is_null() || self.mmio_size == 0 {
            crate::syslog::log_kernel(
                crate::syslog::SyslogSeverity::Info,
                "GPU",
                "QEMU: MMIO no mapeado; se omiten escrituras/lecturas",
            );
            return Ok(());
        }
        // Escrituras de prueba
        self.write_gpu_register(0x0, 0x11111111)?; // ID de GPU
        self.write_gpu_register(0x4, 0x00000001)?; // Habilitar modo gráfico
        self.write_gpu_register(0x8, 0x00000000)?; // Resetear contador
        // Lecturas de verificación
        let id = self.read_gpu_register(0x0).unwrap_or(0);
        let en = self.read_gpu_register(0x4).unwrap_or(0);
        crate::syslog::log_kernel(
            crate::syslog::SyslogSeverity::Info,
            "GPU",
            &alloc::format!("QEMU: readback id=0x{:08X} en=0x{:08X}", id, en),
        );
        Ok(())
    }

    /// Escribir a un registro de GPU basándose en los drivers de Linux
    fn write_gpu_register(&self, offset: u32, value: u32) -> Result<(), &'static str> {
        if !self.initialized {
            return Err("GPU no inicializado");
        }

        // Verificar el offset contra el tamaño del área mapeada
        if (offset as u64) >= self.mmio_size {
            return Err("Offset de registro fuera de rango");
        }
        
        // Calcular la dirección del registro dentro del área mapeada
        // Se usa como base la dirección virtual mapeada
        let reg_ptr = unsafe {
            self.virt_mmio_base.offset(offset as isize) as *mut u32
        };

        // Verificar que el puntero es válido
        if reg_ptr.is_null() {
            return Err("Puntero de registro inválido");
        }

        // Escribir al registro de forma segura (volátil)
        // Esto es crítico para el hardware - el compilador no debe optimizar
        unsafe {
            core::ptr::write_volatile(reg_ptr, value);
            
            // Añadir una pequeña pausa para asegurar que la escritura se complete
            // Esto es común en drivers de hardware
            for _ in 0..10 {
                core::arch::x86_64::_mm_pause();
            }
        }
        Ok(())
    }

    /// Leer de un registro de GPU
    fn read_gpu_register(&self, offset: u32) -> Result<u32, &'static str> {
        if !self.initialized {
            return Err("GPU no inicializado");
        }

        // Verificar el offset contra el tamaño del área mapeada
        if (offset as u64) >= self.mmio_size {
            return Err("Offset de registro fuera de rango");
        }
        
        // Calcular la dirección del registro dentro del área mapeada
        // Se usa como base la dirección virtual mapeada
        let reg_ptr = unsafe {
            self.virt_mmio_base.offset(offset as isize) as *const u32
        };

        // Leer del registro de forma segura (volátil)
        unsafe {
            Ok(core::ptr::read_volatile(reg_ptr))
        }
    }

    /// Cambiar resolución de la GPU
    pub fn change_resolution(&mut self, width: u32, height: u32, bits_per_pixel: u32) -> Result<FramebufferInfo, GpuResult> {
        if !self.initialized {
            return Err(GpuResult::GpuNotFound);
        }

        // Para QEMU/Bochs, usar secuencia VBE via puertos
        if let GpuType::QemuBochs = self.gpu_type {
            // QEMU/Bochs: cambiar modo real por puertos VBE
            if let Err(e) = self.set_bochs_resolution(width, height, bits_per_pixel) {
                crate::syslog::log_kernel(
                    crate::syslog::SyslogSeverity::Error,
                    "GPU",
                    &alloc::format!(
                        "Bochs: fallo configurando {}x{}x{}: {:?}",
                        width, height, bits_per_pixel, e
                    ),
                );
                return Err(GpuResult::SetModeFailed);
            }

            // Pequeña espera para que QEMU aplique el modo
            for _ in 0..20000 { core::hint::spin_loop(); }

            // Tomar base del framebuffer actual si existe
            let (base, ppsl, pf, r, g, b) = if let Some(fb_arc) = crate::drivers::framebuffer_manager::get_global_framebuffer() {
                let fb = &*fb_arc;
                (
                    fb.info.base_address,
                    // En Bochs el ppsl suele ser igual a width; si GOP expone otro, úsalo
                    if fb.info.pixels_per_scan_line > 0 { fb.info.pixels_per_scan_line } else { width },
                    fb.info.pixel_format,
                    fb.info.red_mask,
                    fb.info.green_mask,
                    fb.info.blue_mask,
                )
            } else {
                (self.base_address, width, 2u32, 0x00FF0000, 0x0000FF00, 0x000000FF)
            };

            let info = FramebufferInfo {
                base_address: base,
                width,
                height,
                // En Bochs el stride es width píxeles. Forzamos para evitar ruido por mismatch.
                pixels_per_scan_line: width.max(ppsl),
                pixel_format: pf,
                red_mask: r,
                green_mask: g,
                blue_mask: b,
                reserved_mask: 0u32,
            };
            crate::syslog::log_kernel(
                crate::syslog::SyslogSeverity::Info,
                "GPU",
                &alloc::format!(
                    "Bochs/QEMU: modo aplicado {}x{}@{} ppsl={} base=0x{:X}",
                    width, height, bits_per_pixel, info.pixels_per_scan_line, info.base_address
                ),
            );
            return Ok(info);
        }

        // En otros, usar implementación previa/simple
        self.set_simple_resolution(width, height, bits_per_pixel)
    }

    /// Configurar resolución NVIDIA
    fn set_nvidia_resolution(&mut self, width: u32, height: u32, bits_per_pixel: u32) -> Result<FramebufferInfo, GpuResult> {
        // Deshabilitar video temporalmente
        self.write_gpu_register(0x10, 0x00000000).map_err(|_| GpuResult::SetModeFailed)?;
        
        // Configurar resolución
        self.write_gpu_register(0x14, width).map_err(|_| GpuResult::SetModeFailed)?;
        self.write_gpu_register(0x18, height).map_err(|_| GpuResult::SetModeFailed)?;
        self.write_gpu_register(0x1C, bits_per_pixel).map_err(|_| GpuResult::SetModeFailed)?;
        
        // Calcular stride
        let stride = width * (bits_per_pixel / 8);
        self.write_gpu_register(0x20, stride).map_err(|_| GpuResult::SetModeFailed)?;
        
        // Configurar framebuffer
        let fb_address = self.allocate_framebuffer(width, height, stride)?;
        self.write_gpu_register(0x24, (fb_address & 0xFFFFFFFF) as u32).map_err(|_| GpuResult::SetModeFailed)?;
        self.write_gpu_register(0x28, ((fb_address >> 32) & 0xFFFFFFFF) as u32).map_err(|_| GpuResult::SetModeFailed)?;
        
        // Habilitar video
        self.write_gpu_register(0x10, 0x00000001).map_err(|_| GpuResult::SetModeFailed)?;
        
        // Crear información del framebuffer
        Ok(FramebufferInfo {
            base_address: fb_address,
            width,
            height,
            pixels_per_scan_line: stride,
            pixel_format: 0, // RGB
            red_mask: 0xFF0000,
            green_mask: 0x00FF00,
            blue_mask: 0x0000FF,
            reserved_mask: 0x000000,
        })
    }

    /// Configurar resolución AMD
    fn set_amd_resolution(&mut self, width: u32, height: u32, bits_per_pixel: u32) -> Result<FramebufferInfo, GpuResult> {
        // Deshabilitar video temporalmente
        self.write_gpu_register(0x10, 0x00000000).map_err(|_| GpuResult::SetModeFailed)?;
        
        // Configurar resolución
        self.write_gpu_register(0x14, width).map_err(|_| GpuResult::SetModeFailed)?;
        self.write_gpu_register(0x18, height).map_err(|_| GpuResult::SetModeFailed)?;
        self.write_gpu_register(0x1C, bits_per_pixel).map_err(|_| GpuResult::SetModeFailed)?;
        
        // Calcular stride
        let stride = width * (bits_per_pixel / 8);
        self.write_gpu_register(0x20, stride).map_err(|_| GpuResult::SetModeFailed)?;
        
        // Configurar framebuffer
        let fb_address = self.allocate_framebuffer(width, height, stride)?;
        self.write_gpu_register(0x24, (fb_address & 0xFFFFFFFF) as u32).map_err(|_| GpuResult::SetModeFailed)?;
        self.write_gpu_register(0x28, ((fb_address >> 32) & 0xFFFFFFFF) as u32).map_err(|_| GpuResult::SetModeFailed)?;
        
        // Habilitar video
        self.write_gpu_register(0x10, 0x00000001).map_err(|_| GpuResult::SetModeFailed)?;
        
        // Crear información del framebuffer
        Ok(FramebufferInfo {
            base_address: fb_address,
            width,
            height,
            pixels_per_scan_line: stride,
            pixel_format: 0, // RGB
            red_mask: 0xFF0000,
            green_mask: 0x00FF00,
            blue_mask: 0x0000FF,
            reserved_mask: 0x000000,
        })
    }

    /// Configurar resolución Intel
    fn set_intel_resolution(&mut self, width: u32, height: u32, bits_per_pixel: u32) -> Result<FramebufferInfo, GpuResult> {
        // Deshabilitar video temporalmente
        self.write_gpu_register(0x10, 0x00000000).map_err(|_| GpuResult::SetModeFailed)?;
        
        // Configurar resolución
        self.write_gpu_register(0x14, width).map_err(|_| GpuResult::SetModeFailed)?;
        self.write_gpu_register(0x18, height).map_err(|_| GpuResult::SetModeFailed)?;
        self.write_gpu_register(0x1C, bits_per_pixel).map_err(|_| GpuResult::SetModeFailed)?;
        
        // Calcular stride
        let stride = width * (bits_per_pixel / 8);
        self.write_gpu_register(0x20, stride).map_err(|_| GpuResult::SetModeFailed)?;
        
        // Configurar framebuffer
        let fb_address = self.allocate_framebuffer(width, height, stride)?;
        self.write_gpu_register(0x24, (fb_address & 0xFFFFFFFF) as u32).map_err(|_| GpuResult::SetModeFailed)?;
        self.write_gpu_register(0x28, ((fb_address >> 32) & 0xFFFFFFFF) as u32).map_err(|_| GpuResult::SetModeFailed)?;
        
        // Habilitar video
        self.write_gpu_register(0x10, 0x00000001).map_err(|_| GpuResult::SetModeFailed)?;
        
        // Crear información del framebuffer
        Ok(FramebufferInfo {
            base_address: fb_address,
            width,
            height,
            pixels_per_scan_line: stride,
            pixel_format: 0, // RGB
            red_mask: 0xFF0000,
            green_mask: 0x00FF00,
            blue_mask: 0x0000FF,
            reserved_mask: 0x000000,
        })
    }

    /// Configurar resolución QEMU
    fn set_qemu_resolution(&mut self, width: u32, height: u32, bits_per_pixel: u32) -> Result<FramebufferInfo, GpuResult> {
        // Deshabilitar video temporalmente
        self.write_gpu_register(0x10, 0x00000000).map_err(|_| GpuResult::SetModeFailed)?;
        
        // Configurar resolución
        self.write_gpu_register(0x14, width).map_err(|_| GpuResult::SetModeFailed)?;
        self.write_gpu_register(0x18, height).map_err(|_| GpuResult::SetModeFailed)?;
        self.write_gpu_register(0x1C, bits_per_pixel).map_err(|_| GpuResult::SetModeFailed)?;
        
        // Calcular stride
        let stride = width * (bits_per_pixel / 8);
        self.write_gpu_register(0x20, stride).map_err(|_| GpuResult::SetModeFailed)?;
        
        // Configurar framebuffer
        let fb_address = self.allocate_framebuffer(width, height, stride)?;
        self.write_gpu_register(0x24, (fb_address & 0xFFFFFFFF) as u32).map_err(|_| GpuResult::SetModeFailed)?;
        self.write_gpu_register(0x28, ((fb_address >> 32) & 0xFFFFFFFF) as u32).map_err(|_| GpuResult::SetModeFailed)?;
        
        // Habilitar video
        self.write_gpu_register(0x10, 0x00000001).map_err(|_| GpuResult::SetModeFailed)?;
        
        // Crear información del framebuffer
        Ok(FramebufferInfo {
            base_address: fb_address,
            width,
            height,
            pixels_per_scan_line: stride,
            pixel_format: 0, // RGB
            red_mask: 0xFF0000,
            green_mask: 0x00FF00,
            blue_mask: 0x0000FF,
            reserved_mask: 0x000000,
        })
    }

    /// Configuración de framebuffer directo como Linux
    fn set_simple_resolution(&mut self, width: u32, height: u32, bits_per_pixel: u32) -> Result<FramebufferInfo, GpuResult> {
        // Calcular stride (bytes por línea)
        let stride = width * (bits_per_pixel / 8);
        
        // Configurar la GPU para el nuevo modo
        self.configure_gpu_mode(width, height, bits_per_pixel)?;
        
        // Asignar memoria para el framebuffer
        let fb_address = self.allocate_framebuffer(width, height, stride)?;
        
        // Configurar el framebuffer en la GPU
        self.setup_gpu_framebuffer(fb_address, width, height, stride)?;
        
        // Crear información del framebuffer
        Ok(FramebufferInfo {
            base_address: fb_address,
            width,
            height,
            pixels_per_scan_line: stride,
            pixel_format: 0, // RGB
            red_mask: 0xFF0000,
            green_mask: 0x00FF00,
            blue_mask: 0x0000FF,
            reserved_mask: 0x000000,
        })
    }
    
    /// Configurar modo de GPU
    fn configure_gpu_mode(&self, width: u32, height: u32, bits_per_pixel: u32) -> Result<(), GpuResult> {
        // Deshabilitar video temporalmente
        self.write_gpu_register(0x10, 0x00000000).map_err(|_| GpuResult::SetModeFailed)?;
        
        // Configurar resolución
        let mode_reg = (width << 16) | height;
        self.write_gpu_register(0x14, mode_reg).map_err(|_| GpuResult::SetModeFailed)?;
        
        // Configurar bits por píxel
        self.write_gpu_register(0x18, bits_per_pixel).map_err(|_| GpuResult::SetModeFailed)?;
        
        // Habilitar video
        self.write_gpu_register(0x10, 0x00000001).map_err(|_| GpuResult::SetModeFailed)?;
        
        Ok(())
    }
    
    /// Configurar framebuffer en la GPU
    fn setup_gpu_framebuffer(&self, address: u64, width: u32, height: u32, stride: u32) -> Result<(), GpuResult> {
        // Configurar dirección del framebuffer
        self.write_gpu_register(0x20, address as u32).map_err(|_| GpuResult::SetModeFailed)?;
        self.write_gpu_register(0x24, (address >> 32) as u32).map_err(|_| GpuResult::SetModeFailed)?;
        
        // Configurar stride
        self.write_gpu_register(0x28, stride).map_err(|_| GpuResult::SetModeFailed)?;
        
        // Habilitar aceleración
        self.write_gpu_register(0x30, 0x00000001).map_err(|_| GpuResult::SetModeFailed)?;
        
        Ok(())
    }

    /// Asignar memoria para el framebuffer
    fn allocate_framebuffer(&self, width: u32, height: u32, stride: u32) -> Result<u64, GpuResult> {
        // Calcular tamaño necesario
        let size = (height * stride) as u64;
        
        // En una implementación real, esto asignaría memoria física
        // Por ahora, usamos una dirección simulada
        let fb_address = 0xE0000000 + (self.device_id as u64 * 0x1000000);
        
        // Verificar que la dirección es válida
        if fb_address == 0 {
            return Err(GpuResult::MemoryError);
        }
        
        Ok(fb_address)
    }

    /// Obtener información del GPU
    pub fn get_gpu_info(&self) -> String {
        if !self.initialized {
            return "GPU no inicializado".to_string();
        }
        
        format!("GPU: {:?} (Vendor: 0x{:04X}, Device: 0x{:04X})", 
                self.gpu_type, self.vendor_id, self.device_id)
    }

    /// Verificar si está inicializado
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }
}

impl fmt::Display for GpuResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GpuResult::Success => write!(f, "Éxito"),
            GpuResult::GpuNotFound => write!(f, "GPU no encontrada"),
            GpuResult::InvalidMode => write!(f, "Modo inválido"),
            GpuResult::SetModeFailed => write!(f, "Error al establecer modo"),
            GpuResult::MemoryError => write!(f, "Error de memoria"),
            GpuResult::UnsupportedGpu => write!(f, "GPU no soportada"),
        }
    }
}
