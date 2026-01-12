//! Driver para dispositivos de red VirtIO.

use crate::drivers::pci::PciDevice;
use crate::drivers::framebuffer::{Color, FramebufferDriver}; // Para debug si es necesario
use crate::debug::serial_write_str;
use alloc::alloc::{alloc_zeroed, dealloc, Layout};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::ptr::NonNull;
use spin::Mutex;
use virtio_drivers_and_devices::device::net::VirtIONet;
use virtio_drivers_and_devices::transport::pci::PciTransport;
use virtio_drivers_and_devices::transport::pci::bus::{DeviceFunction, ConfigurationAccess};
use virtio_drivers_and_devices::{BufferDirection, Hal};

use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::time::Instant;

mod pci_helper;

// Estructura para tokens de red
pub struct VirtioRxToken(Vec<u8>);
pub struct VirtioTxToken<'a> {
    driver: &'a VirtioNetDriver,
}

pub struct VirtioNetInner {
    virtio_net: Option<VirtIONet<KernelHal, PciTransport, 32>>,
    mac_address: [u8; 6],
}

pub struct VirtioNetDriver {
    inner: Mutex<VirtioNetInner>,
}

impl VirtioNetDriver {
    pub const fn new_empty() -> Self {
        Self {
            inner: Mutex::new(VirtioNetInner {
                virtio_net: None,
                mac_address: [0; 6],
            }),
        }
    }

    pub fn init(&self, pci_device: PciDevice) -> Result<(), String> {
        serial_write_str("VIRTIO_NET: inicializando driver...\n");
        
        pci_device.enable_mmio_and_bus_master();

        let mut root = pci_helper::PciRootAdapter::new(pci_device)?;
        let device_function = pci_helper::device_function(pci_device);
        
        serial_write_str("VIRTIO_NET: Creando PciTransport...\n");
        let root_mut = root.root_mut();
        
        // PciTransport::new
        let transport = PciTransport::new::<KernelHal, _>(root_mut, device_function)
            .map_err(|e| format!("VirtIO: error creando transporte PCI: {:?}", e))?;

        serial_write_str("VIRTIO_NET: Creando VirtIONet...\n");
        // Tamaño de buffer predeterminado 2048, generic queue size 32
        let mut virtio_net = VirtIONet::<KernelHal, _, 32>::new(transport, 2048)
            .map_err(|e| format!("VirtIONet::new falló: {:?}", e))?;
            
        let mac = virtio_net.mac_address();
        
        serial_write_str(&format!("VIRTIO_NET: MAC Address: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}\n",
            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]));

        let mut inner = self.inner.lock();
        inner.virtio_net = Some(virtio_net);
        inner.mac_address = mac;
        
        Ok(())
    }
    
    pub fn mac_address(&self) -> [u8; 6] {
        self.inner.lock().mac_address
    }
}

// Implementación de Device para &VirtioNetDriver (con interior mutability)
impl<'a> Device for &'a VirtioNetDriver {
    type RxToken<'b> = VirtioRxToken where Self: 'b;
    type TxToken<'b> = VirtioTxToken<'b> where Self: 'b;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let mut inner = self.inner.lock();
        
        if let Some(net) = &mut inner.virtio_net {
            if net.can_recv() {
                // Buffer temporal para recibir
                let mut buffer = [0u8; 2048];
                // Usar receive() en lugar de recv()
                match net.receive() {
                    Ok(rx_buf) => {
                        let data = rx_buf.packet().to_vec();
                        // El buffer se recicla al caerse de scope rx_buf o implícitamente
                        return Some((VirtioRxToken(data), VirtioTxToken { driver: self }));
                    },
                    Err(_e) => {
                        // Error temporal o no hay datos
                        return None;
                    }
                }
            }
        }
        None
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        let inner = self.inner.lock();
        if let Some(net) = &inner.virtio_net {
            if net.can_send() {
                return Some(VirtioTxToken { driver: self });
            }
        }
        None
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = 1500;
        caps
    }
}

impl phy::RxToken for VirtioRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut data = self.0;
        f(&mut data)
    }
}

impl<'a> phy::TxToken for VirtioTxToken<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        // Zero-copy transmit
        let mut inner = self.driver.inner.lock();
        if let Some(net) = &mut inner.virtio_net {
             // Solicitar buffer de transmisión al driver
             let mut tx_buf = net.new_tx_buffer(len);
             // Escribir datos directamente en el buffer DMA
             let result = f(tx_buf.packet_mut());
             // Enviar
             let _ = net.send(tx_buf);
             result
        } else {
             // Fallback si no hay red (no debería pasar si se creó el token)
             let mut buffer = alloc::vec![0u8; len];
             f(&mut buffer)
        }
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

    unsafe fn mmio_phys_to_virt(paddr: usize, _size: usize) -> NonNull<u8> {
        // Mapeo identidad simple (kernel tiene identidad)
        NonNull::new(paddr as *mut u8).unwrap()
    }

    unsafe fn share(buffer: NonNull<[u8]>, _direction: BufferDirection) -> usize {
        buffer.as_ptr() as *const u8 as usize
    }

    unsafe fn unshare(_paddr: usize, _buffer: NonNull<[u8]>, _direction: BufferDirection) {}
}
