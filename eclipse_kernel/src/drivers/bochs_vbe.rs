//! Driver para el dispositivo de gráficos Bochs VBE (usado por QEMU)

use crate::drivers::pci::PciDevice;
use crate::drivers::framebuffer::{FramebufferDriver, FramebufferInfo, Color};
use crate::drivers::ipc::Driver;
use core::ptr;

// Constantes para los puertos I/O de Bochs VBE
const VBE_DISPI_INDEX_PORT: u16 = 0x01CE;
const VBE_DISPI_DATA_PORT: u16 = 0x01CF;

// Índices de registros VBE
const VBE_DISPI_REG_ID: u16 = 0x0;
const VBE_DISPI_REG_XRES: u16 = 0x1;
const VBE_DISPI_REG_YRES: u16 = 0x2;
const VBE_DISPI_REG_BPP: u16 = 0x3;
const VBE_DISPI_REG_ENABLE: u16 = 0x4;
const VBE_DISPI_REG_BANK: u16 = 0x5;
const VBE_DISPI_REG_VIRT_WIDTH: u16 = 0x6;
const VBE_DISPI_REG_VIRT_HEIGHT: u16 = 0x7;
const VBE_DISPI_REG_X_OFFSET: u16 = 0x8;
const VBE_DISPI_REG_Y_OFFSET: u16 = 0x9;

// Valores para el registro de habilitación
const VBE_DISPI_DISABLED: u16 = 0x00;
const VBE_DISPI_ENABLED: u16 = 0x01;
const VBE_DISPI_LFB_ENABLED: u16 = 0x40; // Linear Frame Buffer

#[derive(Debug, Clone)]
pub struct BochsVbeDriver {
    pci_device: PciDevice,
    width: u16,
    height: u16,
    bpp: u16,
    framebuffer: Option<FramebufferDriver>,
    initialized: bool,
}

impl BochsVbeDriver {
    pub fn new(pci_device: PciDevice) -> Self {
        Self {
            pci_device,
            width: 0,
            height: 0,
            bpp: 0,
            framebuffer: None,
            initialized: false,
        }
    }

    pub fn init(&mut self) -> Result<(), &'static str> {
        // En lugar de configurar un nuevo modo, vamos a asumir y usar el que ya está.
        // La información correcta vendrá del framebuffer que nos pase el GpuDriverManager.
        self.initialized = true;
        Ok(())
    }

    fn write_reg(&self, index: u16, value: u16) {
        unsafe {
            x86::io::outw(VBE_DISPI_INDEX_PORT, index);
            x86::io::outw(VBE_DISPI_DATA_PORT, value);
        }
    }

    fn read_reg(&self, index: u16) -> u16 {
        unsafe {
            x86::io::outw(VBE_DISPI_INDEX_PORT, index);
            x86::io::inw(VBE_DISPI_DATA_PORT)
        }
    }

    pub fn get_framebuffer(&mut self) -> Option<&mut FramebufferDriver> {
        self.framebuffer.as_mut()
    }

    pub fn is_ready(&self) -> bool {
        self.initialized
    }
}
