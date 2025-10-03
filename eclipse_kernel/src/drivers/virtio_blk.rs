//! Driver para dispositivos de bloque VirtIO.

use crate::drivers::block::BlockDevice;
use crate::drivers::framebuffer::{Color, FramebufferDriver};
use crate::drivers::pci::PciDevice;
use crate::debug::serial_write_str;
use alloc::alloc::{alloc_zeroed, dealloc, Layout};
use alloc::format;
use alloc::string::String;
use core::ptr::NonNull;
use spin::Mutex;
use virtio_drivers_and_devices::device::blk::{VirtIOBlk, SECTOR_SIZE};
use virtio_drivers_and_devices::transport::pci::PciTransport;
use virtio_drivers_and_devices::transport::pci::bus::{DeviceFunction, ConfigurationAccess};
use virtio_drivers_and_devices::{BufferDirection, Hal};

mod pci_helper;

pub struct VirtioBlkDriver {
    virtio_blk: Mutex<VirtIOBlk<KernelHal, PciTransport>>,
}

impl VirtioBlkDriver {
    pub fn new(pci_device: PciDevice, fb: &mut FramebufferDriver) -> Result<Self, String> {
        serial_write_str("VIRTIO: inicializando driver de bloque...\n");
        serial_write_str(&format!("VIRTIO: PCI device info - vendor_id: 0x{:04x}, device_id: 0x{:04x}, bus: {}, device: {}, function: {}\n", 
            pci_device.vendor_id, pci_device.device_id, pci_device.bus, pci_device.device, pci_device.function));
        
        pci_device.enable_mmio_and_bus_master();

        let mut root = pci_helper::PciRootAdapter::new(pci_device)?;
        let device_function = pci_helper::device_function(pci_device);
        serial_write_str(&format!("VIRTIO: device_function - bus: {}, device: {}, function: {}\n", 
            device_function.bus, device_function.device, device_function.function));
        
        // Verificar que podemos leer el vendor ID antes de crear el transporte
        serial_write_str("VIRTIO: Verificando lectura PCI antes de crear transporte...\n");
        
        // Probar diferentes ubicaciones PCI para encontrar el dispositivo VirtIO
        // Necesitamos acceder al ConfigurationAccess directamente
        serial_write_str("VIRTIO: Creando bridge PCI...\n");
        let bridge = pci_helper::PciMmioBridge::new()?;
        serial_write_str("VIRTIO: Bridge PCI creado, iniciando escaneo...\n");
        for bus in 0..1 {
            for device in 0..8 {
                for function in 0..8 {
                    let test_df = DeviceFunction { bus, device, function };
                    serial_write_str(&format!("VIRTIO: Leyendo bus:{} dev:{} func:{} offset:0x00\n", 
                        bus, device, function));
                    let test_value = bridge.read_word(test_df, 0x00);
                    serial_write_str(&format!("VIRTIO: PCI scan bus:{} dev:{} func:{} vendor_id:0x{:04x} device_id:0x{:04x}\n", 
                        bus, device, function, test_value & 0xFFFF, (test_value >> 16) & 0xFFFF));
                    if test_value != 0x0000_0000 && test_value != 0xFFFF_FFFF {
                        serial_write_str(&format!("VIRTIO: Dispositivo encontrado en bus:{} dev:{} func:{} vendor_id:0x{:04x} device_id:0x{:04x}\n", 
                            bus, device, function, test_value & 0xFFFF, (test_value >> 16) & 0xFFFF));
                    }
                }
            }
        }
        
        serial_write_str("VIRTIO: PciRoot creado correctamente, procediendo con transporte...\n");
        
        let transport = PciTransport::new::<KernelHal, _>(root.root_mut(), device_function)
            .map_err(|e| format!("VirtIO: error creando transporte PCI: {:?}", e))?;

        let virtio_blk = VirtIOBlk::<KernelHal, _>::new(transport)
            .map_err(|e| format!("VirtIOBlk::new falló: {:?}", e))?;

        fb.write_text_kernel("VirtIO-Block inicializado", Color::GREEN);
        serial_write_str("VIRTIO: dispositivo VirtIO-Block inicializado correctamente\n");

        Ok(Self {
            virtio_blk: Mutex::new(virtio_blk),
        })
    }
}

impl BlockDevice for VirtioBlkDriver {
    fn read_blocks(&self, block_address: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        crate::debug::serial_write_str(&alloc::format!(
            "VIRTIO: read_blocks start block={} len={}\n",
            block_address,
            buffer.len()
        ));

        if buffer.is_empty() || buffer.len() % SECTOR_SIZE != 0 {
            crate::debug::serial_write_str("VIRTIO: buffer no alineado o vacío\n");
            return Err("Tamaño de buffer no alineado a sector");
        }

        let mut blk = self.virtio_blk.lock();
        crate::debug::serial_write_str("VIRTIO: lock adquirido, llamando a driver...\n");
        let result = blk.read_blocks(block_address as usize, buffer);
        crate::debug::serial_write_str(&alloc::format!(
            "VIRTIO: read_blocks resultado = {:?}\n",
            result
        ));
        result.map_err(|_| "Error leyendo bloques VirtIO")
    }

    fn write_blocks(&mut self, _block_address: u64, _buffer: &[u8]) -> Result<(), &'static str> {
        Err("Escritura de bloques VirtIO no implementada")
    }

    fn block_size(&self) -> u32 {
        SECTOR_SIZE as u32
    }

    fn block_count(&self) -> u64 {
        self.virtio_blk.lock().capacity()
    }
    
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

struct KernelHal;

unsafe impl Hal for KernelHal {
    fn dma_alloc(pages: usize, _direction: BufferDirection) -> (usize, NonNull<u8>) {
        let size = pages * 4096;
        let layout = Layout::from_size_align(size, 4096).expect("layout DMA");
        let ptr = unsafe { alloc_zeroed(layout) };
        if ptr.is_null() {
            serial_write_str("VIRTIO_HAL: dma_alloc retornó null\n");
            return (0, NonNull::dangling());
        }
        let vaddr = ptr as usize;
        (vaddr, unsafe { NonNull::new_unchecked(ptr) })
    }

    unsafe fn dma_dealloc(_paddr: usize, vaddr: NonNull<u8>, pages: usize) -> i32 {
        let size = pages * 4096;
        let layout = Layout::from_size_align(size, 4096).expect("layout DMA");
        unsafe { dealloc(vaddr.as_ptr(), layout); }
        0
    }

    unsafe fn mmio_phys_to_virt(paddr: usize, size: usize) -> NonNull<u8> {
        let virt = pci_helper::map_mmio_region(paddr as u64, size as u64)
            .expect("map_mmio_region falló");
        NonNull::new(virt as *mut u8).unwrap()
    }

    unsafe fn share(buffer: NonNull<[u8]>, _direction: BufferDirection) -> usize {
        buffer.as_ptr() as *const u8 as usize
    }

    unsafe fn unshare(_paddr: usize, _buffer: NonNull<[u8]>, _direction: BufferDirection) {}
}
