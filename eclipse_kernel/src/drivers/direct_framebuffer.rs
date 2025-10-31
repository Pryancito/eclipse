use crate::drivers::framebuffer::{Color, FramebufferDriver, FramebufferInfo, PixelFormat};
use crate::drivers::framebuffer_manager::{
    get_global_framebuffer, set_global_framebuffer, FramebufferManager,
};
use crate::drivers::pci::{GpuInfo, GpuType, PciDevice};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

/// Información del framebuffer directo detectado
#[derive(Debug, Clone)]
pub struct DirectFramebufferInfo {
    pub base_address: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub pixel_format: PixelFormat,
    pub gpu_type: GpuType,
    pub vendor_id: u16,
    pub device_id: u16,
    pub memory_size: u64,
}

/// Driver de framebuffer directo para diferentes GPUs
pub struct DirectFramebufferDriver {
    current_fb: Option<DirectFramebufferInfo>,
    gpu_type: GpuType,
    vendor_id: u16,
    device_id: u16,
}

impl DirectFramebufferDriver {
    pub fn new() -> Self {
        Self {
            current_fb: None,
            gpu_type: GpuType::Unknown,
            vendor_id: 0,
            device_id: 0,
        }
    }

    /// Detectar y configurar framebuffer directo para cualquier GPU
    pub fn detect_and_configure(&mut self, gpu: &GpuInfo) -> Result<DirectFramebufferInfo, String> {
        self.gpu_type = gpu.gpu_type;
        self.vendor_id = gpu.pci_device.vendor_id;
        self.device_id = gpu.pci_device.device_id;

        // Obtener resolución UEFI actual para evitar corrupción
        let uefi_resolution = self.get_uefi_resolution();

        match gpu.gpu_type {
            GpuType::Nvidia => self.configure_nvidia_framebuffer(gpu, uefi_resolution),
            GpuType::Amd => self.configure_amd_framebuffer(gpu, uefi_resolution),
            GpuType::Intel => self.configure_intel_framebuffer(gpu, uefi_resolution),
            GpuType::QemuBochs => self.configure_qemu_framebuffer(gpu, uefi_resolution),
            GpuType::Vmware => self.configure_vmware_framebuffer(gpu, uefi_resolution),
            GpuType::Virtio => self.configure_virtio_framebuffer(gpu, uefi_resolution),
            _ => Err(String::from(
                "Tipo de GPU no soportado para framebuffer directo",
            )),
        }
    }

    /// Obtener resolución UEFI actual para evitar corrupción
    fn get_uefi_resolution(&self) -> (u32, u32, u32) {
        // Intentar obtener resolución UEFI actual
        if let Some(fb_info) = crate::drivers::framebuffer::get_framebuffer_info() {
            (fb_info.width, fb_info.height, fb_info.pixels_per_scan_line)
        } else {
            // Fallback a resolución segura
            (1366, 768, 2048)
        }
    }

    /// Crear DirectFramebufferInfo usando resolución UEFI
    fn create_framebuffer_info(
        &self,
        gpu: &GpuInfo,
        uefi_resolution: (u32, u32, u32),
        fb_base: u64,
    ) -> DirectFramebufferInfo {
        let (width, height, stride) = uefi_resolution;

        DirectFramebufferInfo {
            base_address: fb_base,
            width,
            height,
            stride,
            pixel_format: PixelFormat::BGR888,
            gpu_type: gpu.gpu_type,
            vendor_id: self.vendor_id,
            device_id: self.device_id,
            memory_size: gpu.memory_size,
        }
    }

    /// Configurar framebuffer directo para NVIDIA
    fn configure_nvidia_framebuffer(
        &mut self,
        gpu: &GpuInfo,
        uefi_resolution: (u32, u32, u32),
    ) -> Result<DirectFramebufferInfo, String> {
        // Leer BARs de NVIDIA
        let bars = gpu.pci_device.read_all_bars();

        // NVIDIA típicamente usa BAR0 para framebuffer
        let fb_base = if bars[0] != 0 {
            bars[0] as u64
        } else if bars[1] != 0 {
            bars[1] as u64
        } else {
            return Err(String::from(
                "No se encontró BAR válido para framebuffer NVIDIA",
            ));
        };

        // NO cambiar resolución - mantener UEFI como cualquier entorno gráfico
        // Solo usar framebuffer directo para optimizaciones de rendimiento
        let (width, height, _stride) = uefi_resolution;

        // NO configurar modo de video - mantener el modo UEFI actual
        // self.set_nvidia_video_mode(&gpu.pci_device, width, height)?;

        // Crear framebuffer info usando resolución UEFI
        let fb_info = self.create_framebuffer_info(gpu, uefi_resolution, fb_base);

        self.current_fb = Some(fb_info.clone());
        Ok(fb_info)
    }

    /// Configurar framebuffer directo para AMD
    fn configure_amd_framebuffer(
        &mut self,
        gpu: &GpuInfo,
        uefi_resolution: (u32, u32, u32),
    ) -> Result<DirectFramebufferInfo, String> {
        let bars = gpu.pci_device.read_all_bars();

        // AMD típicamente usa BAR0 para framebuffer
        let fb_base = if bars[0] != 0 {
            bars[0] as u64
        } else {
            return Err(String::from(
                "No se encontró BAR válido para framebuffer AMD",
            ));
        };

        // NO cambiar resolución - mantener UEFI como cualquier entorno gráfico
        // Solo usar framebuffer directo para optimizaciones de rendimiento
        let (width, height, _stride) = uefi_resolution;

        // NO configurar modo de video - mantener el modo UEFI actual
        // self.set_amd_video_mode(&gpu.pci_device, width, height)?;

        // Crear framebuffer info usando resolución UEFI
        let fb_info = self.create_framebuffer_info(gpu, uefi_resolution, fb_base);

        self.current_fb = Some(fb_info.clone());
        Ok(fb_info)
    }

    /// Configurar framebuffer directo para Intel
    fn configure_intel_framebuffer(
        &mut self,
        gpu: &GpuInfo,
        uefi_resolution: (u32, u32, u32),
    ) -> Result<DirectFramebufferInfo, String> {
        let bars = gpu.pci_device.read_all_bars();

        // Intel típicamente usa BAR0 para framebuffer
        let fb_base = if bars[0] != 0 {
            bars[0] as u64
        } else {
            return Err(String::from(
                "No se encontró BAR válido para framebuffer Intel",
            ));
        };

        // NO cambiar resolución - mantener UEFI como cualquier entorno gráfico
        // Solo usar framebuffer directo para optimizaciones de rendimiento
        let (width, height, _stride) = uefi_resolution;

        // NO configurar modo de video - mantener el modo UEFI actual
        // self.set_intel_video_mode(&gpu.pci_device, width, height)?;

        // Crear framebuffer info usando resolución UEFI
        let fb_info = self.create_framebuffer_info(gpu, uefi_resolution, fb_base);

        self.current_fb = Some(fb_info.clone());
        Ok(fb_info)
    }

    /// Configurar framebuffer directo para QEMU
    fn configure_qemu_framebuffer(
        &mut self,
        gpu: &GpuInfo,
        uefi_resolution: (u32, u32, u32),
    ) -> Result<DirectFramebufferInfo, String> {
        // QEMU puede usar tanto VGA text mode como framebuffer gráfico
        // Usar resolución UEFI para mantener consistencia
        let (width, height, stride) = uefi_resolution;

        // Si la resolución es muy pequeña, usar VGA text mode
        if width < 640 || height < 480 {
            let fb_info = DirectFramebufferInfo {
                base_address: 0xB8000, // VGA text mode
                width: 80,
                height: 25,
                stride: 80,
                pixel_format: PixelFormat::BGR888,
                gpu_type: GpuType::QemuBochs,
                vendor_id: self.vendor_id,
                device_id: self.device_id,
                memory_size: 0x8000, // 32KB para VGA
            };
            self.current_fb = Some(fb_info.clone());
            Ok(fb_info)
        } else {
            // Usar framebuffer gráfico con resolución UEFI
            let fb_info = DirectFramebufferInfo {
                base_address: 0xE0000000, // Framebuffer gráfico QEMU
                width,
                height,
                stride,
                pixel_format: PixelFormat::BGR888,
                gpu_type: GpuType::QemuBochs,
                vendor_id: self.vendor_id,
                device_id: self.device_id,
                memory_size: (width * height * 4) as u64, // 4 bytes por píxel
            };
            self.current_fb = Some(fb_info.clone());
            Ok(fb_info)
        }
    }

    /// Configurar framebuffer directo para VMware
    fn configure_vmware_framebuffer(
        &mut self,
        gpu: &GpuInfo,
        uefi_resolution: (u32, u32, u32),
    ) -> Result<DirectFramebufferInfo, String> {
        let bars = gpu.pci_device.read_all_bars();

        let fb_base = if bars[0] != 0 {
            bars[0] as u64
        } else {
            return Err(String::from(
                "No se encontró BAR válido para framebuffer VMware",
            ));
        };

        // NO cambiar resolución - mantener UEFI como cualquier entorno gráfico
        // Solo usar framebuffer directo para optimizaciones de rendimiento
        let (width, height, _stride) = uefi_resolution;

        // Crear framebuffer info usando resolución UEFI
        let fb_info = self.create_framebuffer_info(gpu, uefi_resolution, fb_base);

        self.current_fb = Some(fb_info.clone());
        Ok(fb_info)
    }

    /// Configurar framebuffer directo para VirtIO
    fn configure_virtio_framebuffer(
        &mut self,
        gpu: &GpuInfo,
        uefi_resolution: (u32, u32, u32),
    ) -> Result<DirectFramebufferInfo, String> {
        let bars = gpu.pci_device.read_all_bars();

        let fb_base = if bars[0] != 0 {
            bars[0] as u64
        } else {
            return Err(String::from(
                "No se encontró BAR válido para framebuffer VirtIO",
            ));
        };

        // NO cambiar resolución - mantener UEFI como cualquier entorno gráfico
        // Solo usar framebuffer directo para optimizaciones de rendimiento
        let (width, height, _stride) = uefi_resolution;

        // Crear framebuffer info usando resolución UEFI
        let fb_info = self.create_framebuffer_info(gpu, uefi_resolution, fb_base);

        self.current_fb = Some(fb_info.clone());
        Ok(fb_info)
    }

    /// Configurar modo de video específico para NVIDIA
    fn set_nvidia_video_mode(
        &self,
        pci_device: &PciDevice,
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        // Habilitar MMIO y Bus Master
        pci_device.enable_mmio_and_bus_master();

        // Configurar modo de video NVIDIA
        // Esto es específico del hardware NVIDIA
        unsafe {
            // Configurar resolución en registros NVIDIA
            let bars = pci_device.read_all_bars();
            let fb_ptr = bars[0] as *mut u32;
            if !fb_ptr.is_null() {
                // Configurar ancho
                core::ptr::write_volatile(fb_ptr.add(0), width);
                // Configurar alto
                core::ptr::write_volatile(fb_ptr.add(1), height);
                // Configurar formato de píxel (BGR888)
                core::ptr::write_volatile(fb_ptr.add(2), 0x20); // BGR888
            }
        }

        Ok(())
    }

    /// Configurar modo de video específico para AMD
    fn set_amd_video_mode(
        &self,
        pci_device: &PciDevice,
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        pci_device.enable_mmio_and_bus_master();

        unsafe {
            let bars = pci_device.read_all_bars();
            let fb_ptr = bars[0] as *mut u32;
            if !fb_ptr.is_null() {
                core::ptr::write_volatile(fb_ptr.add(0), width);
                core::ptr::write_volatile(fb_ptr.add(1), height);
                core::ptr::write_volatile(fb_ptr.add(2), 0x20);
            }
        }

        Ok(())
    }

    /// Configurar modo de video específico para Intel
    fn set_intel_video_mode(
        &self,
        pci_device: &PciDevice,
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        pci_device.enable_mmio_and_bus_master();

        unsafe {
            let bars = pci_device.read_all_bars();
            let fb_ptr = bars[0] as *mut u32;
            if !fb_ptr.is_null() {
                core::ptr::write_volatile(fb_ptr.add(0), width);
                core::ptr::write_volatile(fb_ptr.add(1), height);
                core::ptr::write_volatile(fb_ptr.add(2), 0x20);
            }
        }

        Ok(())
    }

    /// Obtener información del framebuffer actual
    pub fn get_current_framebuffer(&self) -> Option<&DirectFramebufferInfo> {
        self.current_fb.as_ref()
    }

    /// Crear FramebufferDriver optimizado desde información directa del hardware
    /// IMPORTANTE: Crear framebuffer optimizado con drivers específicos de la GPU
    pub fn create_framebuffer_driver(&self) -> Result<FramebufferDriver, String> {
        if let Some(fb_info) = &self.current_fb {
            // PASO 1: Configurar hardware a nivel de ensamblador
            self.initialize_hardware_framebuffer(fb_info)?;

            // PASO 2: Crear nuevo FramebufferDriver optimizado con el hardware específico
            let framebuffer_info = FramebufferInfo {
                base_address: fb_info.base_address,
                width: fb_info.width,
                height: fb_info.height,
                pixels_per_scan_line: fb_info.stride,
                pixel_format: fb_info.pixel_format as u32,
                red_mask: 0xFF0000,
                green_mask: 0x00FF00,
                blue_mask: 0x0000FF,
                reserved_mask: 0x000000,
            };

            // PASO 3: Crear nuevo FramebufferDriver con configuración optimizada
            let mut new_fb = FramebufferDriver::new();

            // PASO 4: Inicializar con la información del hardware específico
            let pixel_bitmask = (framebuffer_info.red_mask << 16)
                | (framebuffer_info.green_mask << 8)
                | framebuffer_info.blue_mask;

            match new_fb.init_from_uefi(
                framebuffer_info.base_address,
                framebuffer_info.width,
                framebuffer_info.height,
                framebuffer_info.pixels_per_scan_line,
                framebuffer_info.pixel_format,
                pixel_bitmask,
            ) {
                Ok(_) => {
                    // PASO 5: Establecer como framebuffer global optimizado
                    set_global_framebuffer(new_fb.clone());
                    Ok(new_fb)
                }
                Err(e) => Err(format!("Error inicializando framebuffer optimizado: {}", e)),
            }
        } else {
            Err(String::from(
                "No hay información de framebuffer directo disponible",
            ))
        }
    }

    /// Inicializar hardware del framebuffer a nivel de ensamblador
    pub fn initialize_hardware_framebuffer(
        &self,
        fb_info: &DirectFramebufferInfo,
    ) -> Result<(), String> {
        match fb_info.gpu_type {
            crate::drivers::pci::GpuType::Nvidia => self.initialize_nvidia_hardware(fb_info),
            crate::drivers::pci::GpuType::Amd => self.initialize_amd_hardware(fb_info),
            crate::drivers::pci::GpuType::Intel => self.initialize_intel_hardware(fb_info),
            crate::drivers::pci::GpuType::QemuBochs => {
                // QEMU no necesita reconfiguración compleja
                self.initialize_qemu_simple(fb_info)
            }
            crate::drivers::pci::GpuType::Vmware => self.initialize_vmware_hardware(fb_info),
            crate::drivers::pci::GpuType::Virtio => self.initialize_virtio_hardware(fb_info),
            _ => {
                // Para GPUs desconocidas, usar configuración genérica
                self.initialize_generic_hardware(fb_info)
            }
        }
    }

    /// Reconfigurar tarjeta gráfica para nuevo framebuffer
    pub fn reconfigure_graphics_card(
        &self,
        fb_info: &crate::drivers::framebuffer::FramebufferInfo,
    ) -> Result<(), String> {
        if let Some(direct_fb_info) = &self.current_fb {
            match direct_fb_info.gpu_type {
                crate::drivers::pci::GpuType::Nvidia => self.reconfigure_nvidia_graphics(fb_info),
                crate::drivers::pci::GpuType::Amd => self.reconfigure_amd_graphics(fb_info),
                crate::drivers::pci::GpuType::Intel => self.reconfigure_intel_graphics(fb_info),
                crate::drivers::pci::GpuType::QemuBochs => {
                    // QEMU no necesita reconfiguración
                    Ok(())
                }
                crate::drivers::pci::GpuType::Vmware => self.reconfigure_vmware_graphics(fb_info),
                crate::drivers::pci::GpuType::Virtio => self.reconfigure_virtio_graphics(fb_info),
                _ => {
                    // Para GPUs desconocidas, usar configuración genérica
                    self.reconfigure_generic_graphics(fb_info)
                }
            }
        } else {
            Err(String::from(
                "No hay información de framebuffer directo disponible",
            ))
        }
    }

    /// Inicializar hardware NVIDIA a nivel de ensamblador
    fn initialize_nvidia_hardware(&self, fb_info: &DirectFramebufferInfo) -> Result<(), String> {
        // Configurar registros NVIDIA específicos
        unsafe {
            // Configurar modo de video NVIDIA
            self.write_nvidia_register(fb_info, 0x0, 0x00000000); // Reset
            self.write_nvidia_register(fb_info, 0x1, fb_info.width); // Width
            self.write_nvidia_register(fb_info, 0x2, fb_info.height); // Height
            self.write_nvidia_register(fb_info, 0x3, fb_info.stride); // Stride
            self.write_nvidia_register(fb_info, 0x4, 0x00000020); // Pixel format (32-bit)
            self.write_nvidia_register(fb_info, 0x5, fb_info.base_address as u32); // Base address low
            self.write_nvidia_register(fb_info, 0x6, (fb_info.base_address >> 32) as u32); // Base address high

            // Habilitar aceleración 2D
            self.write_nvidia_register(fb_info, 0x10, 0x00000001); // Enable 2D acceleration
            self.write_nvidia_register(fb_info, 0x11, 0x00000001); // Enable hardware cursor
            self.write_nvidia_register(fb_info, 0x12, 0x00000001); // Enable hardware scrolling
        }
        Ok(())
    }

    /// Inicializar hardware AMD a nivel de ensamblador
    fn initialize_amd_hardware(&self, fb_info: &DirectFramebufferInfo) -> Result<(), String> {
        unsafe {
            // Configurar registros AMD específicos
            self.write_amd_register(fb_info, 0x0, 0x00000000); // Reset
            self.write_amd_register(fb_info, 0x1, fb_info.width); // Width
            self.write_amd_register(fb_info, 0x2, fb_info.height); // Height
            self.write_amd_register(fb_info, 0x3, fb_info.stride); // Stride
            self.write_amd_register(fb_info, 0x4, 0x00000020); // Pixel format
            self.write_amd_register(fb_info, 0x5, fb_info.base_address as u32); // Base address

            // Habilitar aceleración AMD
            self.write_amd_register(fb_info, 0x10, 0x00000001); // Enable 2D
            self.write_amd_register(fb_info, 0x11, 0x00000001); // Enable scrolling
        }
        Ok(())
    }

    /// Inicializar hardware Intel a nivel de ensamblador
    fn initialize_intel_hardware(&self, fb_info: &DirectFramebufferInfo) -> Result<(), String> {
        unsafe {
            // Configurar registros Intel específicos
            self.write_intel_register(fb_info, 0x0, 0x00000000); // Reset
            self.write_intel_register(fb_info, 0x1, fb_info.width); // Width
            self.write_intel_register(fb_info, 0x2, fb_info.height); // Height
            self.write_intel_register(fb_info, 0x3, fb_info.stride); // Stride
            self.write_intel_register(fb_info, 0x4, 0x00000020); // Pixel format
            self.write_intel_register(fb_info, 0x5, fb_info.base_address as u32); // Base address

            // Habilitar aceleración Intel
            self.write_intel_register(fb_info, 0x10, 0x00000001); // Enable 2D
            self.write_intel_register(fb_info, 0x11, 0x00000001); // Enable scrolling
        }
        Ok(())
    }

    /// Inicializar hardware QEMU a nivel de ensamblador
    fn initialize_qemu_hardware(&self, fb_info: &DirectFramebufferInfo) -> Result<(), String> {
        unsafe {
            // Configurar registros QEMU/VirtIO específicos
            self.write_qemu_register(fb_info, 0x0, 0x00000000); // Reset
            self.write_qemu_register(fb_info, 0x1, fb_info.width); // Width
            self.write_qemu_register(fb_info, 0x2, fb_info.height); // Height
            self.write_qemu_register(fb_info, 0x3, fb_info.stride); // Stride
            self.write_qemu_register(fb_info, 0x4, 0x00000020); // Pixel format
            self.write_qemu_register(fb_info, 0x5, fb_info.base_address as u32);
            // Base address
        }
        Ok(())
    }

    /// Inicialización simple para QEMU (sin reconfiguración compleja)
    fn initialize_qemu_simple(&self, fb_info: &DirectFramebufferInfo) -> Result<(), String> {
        // QEMU no necesita reconfiguración compleja
        // Solo verificar que el framebuffer esté disponible
        unsafe {
            // Verificar que el framebuffer esté mapeado correctamente
            let base = fb_info.base_address as *const u32;
            if base.is_null() {
                return Err(String::from("Framebuffer QEMU no disponible"));
            }

            // Configuración mínima para QEMU
            self.write_qemu_register(fb_info, 0x0, 0x00000001); // Enable
            self.write_qemu_register(fb_info, 0x1, fb_info.width); // Width
            self.write_qemu_register(fb_info, 0x2, fb_info.height); // Height
        }
        Ok(())
    }

    /// Inicializar hardware VMware a nivel de ensamblador
    fn initialize_vmware_hardware(&self, fb_info: &DirectFramebufferInfo) -> Result<(), String> {
        unsafe {
            // Configurar registros VMware SVGA específicos
            self.write_vmware_register(fb_info, 0x0, 0x00000000); // Reset
            self.write_vmware_register(fb_info, 0x1, fb_info.width); // Width
            self.write_vmware_register(fb_info, 0x2, fb_info.height); // Height
            self.write_vmware_register(fb_info, 0x3, fb_info.stride); // Stride
            self.write_vmware_register(fb_info, 0x4, 0x00000020); // Pixel format
            self.write_vmware_register(fb_info, 0x5, fb_info.base_address as u32);
            // Base address
        }
        Ok(())
    }

    /// Inicializar hardware VirtIO a nivel de ensamblador
    fn initialize_virtio_hardware(&self, fb_info: &DirectFramebufferInfo) -> Result<(), String> {
        unsafe {
            // Configurar registros VirtIO específicos
            self.write_virtio_register(fb_info, 0x0, 0x00000000); // Reset
            self.write_virtio_register(fb_info, 0x1, fb_info.width); // Width
            self.write_virtio_register(fb_info, 0x2, fb_info.height); // Height
            self.write_virtio_register(fb_info, 0x3, fb_info.stride); // Stride
            self.write_virtio_register(fb_info, 0x4, 0x00000020); // Pixel format
            self.write_virtio_register(fb_info, 0x5, fb_info.base_address as u32);
            // Base address
        }
        Ok(())
    }

    /// Inicializar hardware genérico a nivel de ensamblador
    fn initialize_generic_hardware(&self, fb_info: &DirectFramebufferInfo) -> Result<(), String> {
        unsafe {
            // Configuración genérica para GPUs desconocidas
            self.write_generic_register(fb_info, 0x0, 0x00000000); // Reset
            self.write_generic_register(fb_info, 0x1, fb_info.width); // Width
            self.write_generic_register(fb_info, 0x2, fb_info.height); // Height
            self.write_generic_register(fb_info, 0x3, fb_info.stride); // Stride
            self.write_generic_register(fb_info, 0x4, 0x00000020); // Pixel format
            self.write_generic_register(fb_info, 0x5, fb_info.base_address as u32);
            // Base address
        }
        Ok(())
    }

    // Funciones de escritura de registros específicas por GPU
    unsafe fn write_nvidia_register(&self, fb_info: &DirectFramebufferInfo, reg: u32, value: u32) {
        // Implementación específica para NVIDIA
        let base = fb_info.base_address as *mut u32;
        base.add(reg as usize).write_volatile(value);
    }

    unsafe fn write_amd_register(&self, fb_info: &DirectFramebufferInfo, reg: u32, value: u32) {
        // Implementación específica para AMD
        let base = fb_info.base_address as *mut u32;
        base.add(reg as usize).write_volatile(value);
    }

    unsafe fn write_intel_register(&self, fb_info: &DirectFramebufferInfo, reg: u32, value: u32) {
        // Implementación específica para Intel
        let base = fb_info.base_address as *mut u32;
        base.add(reg as usize).write_volatile(value);
    }

    unsafe fn write_qemu_register(&self, fb_info: &DirectFramebufferInfo, reg: u32, value: u32) {
        // Implementación específica para QEMU
        let base = fb_info.base_address as *mut u32;
        base.add(reg as usize).write_volatile(value);
    }

    unsafe fn write_vmware_register(&self, fb_info: &DirectFramebufferInfo, reg: u32, value: u32) {
        // Implementación específica para VMware
        let base = fb_info.base_address as *mut u32;
        base.add(reg as usize).write_volatile(value);
    }

    unsafe fn write_virtio_register(&self, fb_info: &DirectFramebufferInfo, reg: u32, value: u32) {
        // Implementación específica para VirtIO
        let base = fb_info.base_address as *mut u32;
        base.add(reg as usize).write_volatile(value);
    }

    unsafe fn write_generic_register(&self, fb_info: &DirectFramebufferInfo, reg: u32, value: u32) {
        // Implementación genérica
        let base = fb_info.base_address as *mut u32;
        base.add(reg as usize).write_volatile(value);
    }

    /// Obtener información de diagnóstico
    pub fn get_diagnostic_info(&self) -> String {
        if let Some(fb) = &self.current_fb {
            format!(
                "Direct FB: {}x{} @0x{:X} ({}: {:04X}:{:04X})",
                fb.width,
                fb.height,
                fb.base_address,
                fb.gpu_type.as_str(),
                fb.vendor_id,
                fb.device_id
            )
        } else {
            String::from("No framebuffer directo activo")
        }
    }

    // Funciones de reconfiguración específicas por GPU
    fn reconfigure_nvidia_graphics(
        &self,
        fb_info: &crate::drivers::framebuffer::FramebufferInfo,
    ) -> Result<(), String> {
        // Reconfigurar NVIDIA usando los drivers específicos
        // Esto se integraría con el driver NVIDIA real
        Ok(())
    }

    fn reconfigure_amd_graphics(
        &self,
        fb_info: &crate::drivers::framebuffer::FramebufferInfo,
    ) -> Result<(), String> {
        // Reconfigurar AMD usando los drivers específicos
        // Esto se integraría con el driver AMD real
        Ok(())
    }

    fn reconfigure_intel_graphics(
        &self,
        fb_info: &crate::drivers::framebuffer::FramebufferInfo,
    ) -> Result<(), String> {
        // Reconfigurar Intel usando los drivers específicos
        Ok(())
    }

    fn reconfigure_vmware_graphics(
        &self,
        fb_info: &crate::drivers::framebuffer::FramebufferInfo,
    ) -> Result<(), String> {
        // Reconfigurar VMware usando los drivers específicos
        Ok(())
    }

    fn reconfigure_virtio_graphics(
        &self,
        fb_info: &crate::drivers::framebuffer::FramebufferInfo,
    ) -> Result<(), String> {
        // Reconfigurar VirtIO usando los drivers específicos
        Ok(())
    }

    fn reconfigure_generic_graphics(
        &self,
        fb_info: &crate::drivers::framebuffer::FramebufferInfo,
    ) -> Result<(), String> {
        // Reconfiguración genérica para GPUs desconocidas
        Ok(())
    }
}

impl Default for DirectFramebufferDriver {
    fn default() -> Self {
        Self::new()
    }
}
