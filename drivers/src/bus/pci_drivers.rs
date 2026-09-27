//! Registro modular de drivers PCI.
//!
//! Cada driver implementa el trait [`PciDriver`] y se registra en [`init_all`].
//! La función [`probe_pci_device`] itera el registro para encontrar el driver
//! adecuado para cada dispositivo PCI detectado.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use lock::Mutex;
use pci::PCIDevice;

use crate::builder::IoMapper;
use crate::{Device, DeviceResult};

/// Interfaz que debe implementar cada driver PCI modular.
pub trait PciDriver: Send + Sync {
    /// Nombre descriptivo del driver (solo para log/debug).
    fn name(&self) -> &str;

    /// Devuelve `true` si este driver gestiona el par (vendor_id, device_id).
    fn matched(&self, vendor_id: u16, device_id: u16) -> bool;

    /// Devuelve `true` si este driver gestiona el dispositivo dado.
    ///
    /// La implementación por defecto delega en [`matched`](PciDriver::matched)
    /// pasando los ids de `dev`. Los drivers que necesiten inspeccionar
    /// class/subclass/prog_if deben sobreescribir este método.
    fn matched_dev(&self, dev: &PCIDevice) -> bool {
        self.matched(dev.id.vendor_id, dev.id.device_id)
    }

    /// Inicializa el dispositivo y devuelve el [`Device`] creado.
    fn init(
        &self,
        dev: &PCIDevice,
        mapper: &Option<Arc<dyn IoMapper>>,
        irq: Option<usize>,
    ) -> DeviceResult<Device>;
}

// ——— Registro global ———

static DRIVERS: Mutex<Vec<&'static (dyn PciDriver + Send + Sync)>> = Mutex::new(Vec::new());

/// Whether [`init_all`] has already filled the registry.
///
/// There was nothing here: a second call registered every driver a second
/// time, and [`probe_pci_device`] then offered each device to the same driver
/// twice. The second bring-up runs against hardware the first one already
/// took, and its `Device` --- queues, interrupt handlers and DMA included ---
/// is the one the caller keeps.
static REGISTERED: AtomicBool = AtomicBool::new(false);

fn register(driver: &'static (dyn PciDriver + Send + Sync)) {
    DRIVERS.lock().push(driver);
}

/// Claim the one-time registration: `true` the first time, `false` ever after.
fn claim_registration() -> bool {
    !REGISTERED.swap(true, Ordering::SeqCst)
}

/// Registra todos los drivers PCI conocidos.
///
/// Debe llamarse una vez en tiempo de inicialización, antes de enumerar el bus
/// PCI. Una segunda llamada no hace nada.
pub fn init_all() {
    if !claim_registration() {
        warn!("[pci] init_all llamado dos veces; el registro ya tiene sus drivers");
        return;
    }
    // NVMe
    register(&crate::nvme::interface::NvmeDriverPci);

    // AHCI / SATA
    register(&crate::ata::ahci::AhciDriverPci);

    // Ethernet Intel e1000 / e1000e
    register(&crate::net::e1000::E1000DriverPci);
    register(&crate::net::e1000e::E1000eDriverPci);

    // GPU Nvidia (scaffolding) — x86_64-only (nvidia-rm-sys shim).
    //
    // Out of the host test build: its `init` reaches `nvidia-rm-sys`, whose
    // vendored NVIDIA C is only compiled for the kernel target, so a test that
    // calls `init_all` fails to link with `undefined symbol: fnv1Hash64`
    // instead of running. Everything else in the registry is here under test.
    #[cfg(all(target_arch = "x86_64", not(test)))]
    register(&crate::display::NvidiaGpuDriverPci);

    // HD Audio (PCH onboard + NVIDIA GPU HDMI audio functions)
    register(&crate::audio::hda::HdaDriverPci);

    // xHCI USB HID (teclado/ratón/tablet)
    #[cfg(all(
        any(feature = "xhci-usb-hid", feature = "legacy-usb-hid"),
        target_arch = "x86_64",
        not(feature = "mock"),
        not(feature = "no-pci")
    ))]
    register(&crate::usb::xhci_hid::XhciDriverPci);
}

/// Intenta inicializar `dev` con el primer driver del registro que lo soporte.
///
/// Devuelve el primer error de verdad si algún driver que coincidía falló, y
/// `Err(DeviceError::NotSupported)` si ninguno coincide o todos declinan.
pub fn probe_pci_device(
    dev: &PCIDevice,
    mapper: &Option<Arc<dyn IoMapper>>,
    irq: Option<usize>,
) -> DeviceResult<Device> {
    // A snapshot, because the registry must not stay locked while a driver
    // brings hardware up: `init` is arbitrary driver code --- it maps BARs,
    // waits for a PHY to negotiate, registers interrupt handlers --- and
    // `DRIVERS` is a spin lock that every other CPU enumerating PCI wants. A
    // driver whose `init` reached the registry, its own sub-driver or a nested
    // probe, hung the boot with nothing printed.
    let drivers: Vec<&'static (dyn PciDriver + Send + Sync)> =
        DRIVERS.lock().iter().copied().collect();

    // The first real failure, kept so that it does not come back as "no driver
    // for this device": the caller cannot tell `NotSupported` from "the NVMe
    // driver could not get DMA memory", and quietly moves on to the legacy
    // path as if the device were unknown.
    let mut failure = None;
    for drv in drivers {
        if !drv.matched_dev(dev) {
            continue;
        }
        match drv.init(dev, mapper, irq) {
            Ok(device) => {
                info!("[pci] {} inicializado correctamente", drv.name());
                return Ok(device);
            }
            Err(crate::DeviceError::NotSupported) => {
                // Matched the class but declined this function (e.g. extra
                // NVIDIA HDMI on a GPU with no monitor). Not a failure.
                debug!("[pci] driver '{}' skipped (NotSupported)", drv.name());
            }
            Err(e) => {
                warn!("[pci] driver '{}' falló: {:?}", drv.name(), e);
                failure = failure.or(Some(e));
            }
        }
    }
    Err(failure.unwrap_or(crate::DeviceError::NotSupported))
}

/// Swap the registry's contents, returning what was there.
#[cfg(test)]
fn replace_drivers(
    new: Vec<&'static (dyn PciDriver + Send + Sync)>,
) -> Vec<&'static (dyn PciDriver + Send + Sync)> {
    core::mem::replace(&mut *DRIVERS.lock(), new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheme::{BlockScheme, Scheme};
    use crate::DeviceError;
    use core::sync::atomic::AtomicUsize;
    use pci::{
        CSpaceAccessMethod, Command, DeviceDetails, DeviceKind, Identifier, Location, Status,
    };
    extern crate std;

    /// The registry and the "already registered" flag are process-wide statics,
    /// so the tests take turns and each leaves them as it found them.
    fn alone_with_the_registry<R>(body: impl FnOnce() -> R) -> R {
        static TURNSTILE: Mutex<()> = Mutex::new(());
        let _guard = TURNSTILE.lock();
        let saved = replace_drivers(Vec::new());
        let was_registered = REGISTERED.swap(false, Ordering::SeqCst);
        let out = body();
        replace_drivers(saved);
        REGISTERED.store(was_registered, Ordering::SeqCst);
        out
    }

    /// A block device that exists only to be the `Device` a fake driver hands
    /// back.
    struct Nothing;

    impl Scheme for Nothing {
        fn name(&self) -> &str {
            "nothing"
        }
    }

    impl BlockScheme for Nothing {
        fn read_block(&self, _block_id: usize, _buf: &mut [u8]) -> DeviceResult {
            Ok(())
        }
        fn write_block(&self, _block_id: usize, _buf: &[u8]) -> DeviceResult {
            Ok(())
        }
        fn flush(&self) -> DeviceResult {
            Ok(())
        }
        fn block_count(&self) -> usize {
            0
        }
    }

    #[derive(Clone, Copy)]
    enum Answer {
        /// Brings the device up.
        Works,
        /// Matched the class but declined this function.
        Declines,
        /// Failed for a reason of its own.
        Fails(DeviceError),
        /// Reaches the registry from inside `init`, the way a driver that
        /// registers a sub-driver or probes a nested device would.
        TouchesTheRegistry,
    }

    struct Fake {
        name: &'static str,
        ids: (u16, u16),
        answer: Answer,
        calls: &'static AtomicUsize,
    }

    impl PciDriver for Fake {
        fn name(&self) -> &str {
            self.name
        }

        fn matched(&self, vendor_id: u16, device_id: u16) -> bool {
            (vendor_id, device_id) == self.ids
        }

        fn init(
            &self,
            _dev: &PCIDevice,
            _mapper: &Option<Arc<dyn IoMapper>>,
            _irq: Option<usize>,
        ) -> DeviceResult<Device> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.answer {
                Answer::Works => Ok(Device::Block(Arc::new(Nothing))),
                Answer::Declines => Err(DeviceError::NotSupported),
                Answer::Fails(e) => Err(e),
                Answer::TouchesTheRegistry => match DRIVERS.try_lock() {
                    // A spin lock still held by `probe_pci_device` would not
                    // come back at all in the kernel: here it says so instead
                    // of hanging the suite.
                    Some(_) => Ok(Device::Block(Arc::new(Nothing))),
                    None => Err(DeviceError::IoError),
                },
            }
        }
    }

    /// A driver that decides by class, overriding `matched_dev`, so the default
    /// implementation's use of the ids is a separate question.
    struct ByClass {
        class: u8,
        calls: &'static AtomicUsize,
    }

    impl PciDriver for ByClass {
        fn name(&self) -> &str {
            "by-class"
        }
        fn matched(&self, _vendor_id: u16, _device_id: u16) -> bool {
            false
        }
        fn matched_dev(&self, dev: &PCIDevice) -> bool {
            dev.id.class == self.class
        }
        fn init(
            &self,
            _dev: &PCIDevice,
            _mapper: &Option<Arc<dyn IoMapper>>,
            _irq: Option<usize>,
        ) -> DeviceResult<Device> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Device::Block(Arc::new(Nothing)))
        }
    }

    fn counter() -> &'static AtomicUsize {
        alloc::boxed::Box::leak(alloc::boxed::Box::new(AtomicUsize::new(0)))
    }

    fn fake(
        name: &'static str,
        ids: (u16, u16),
        answer: Answer,
    ) -> (&'static (dyn PciDriver + Send + Sync), &'static AtomicUsize) {
        let calls = counter();
        let drv = alloc::boxed::Box::leak(alloc::boxed::Box::new(Fake {
            name,
            ids,
            answer,
            calls,
        }));
        (drv, calls)
    }

    fn device(vendor_id: u16, device_id: u16) -> PCIDevice {
        PCIDevice {
            loc: Location {
                bus: 0,
                device: 3,
                function: 0,
            },
            id: Identifier {
                vendor_id,
                device_id,
                revision_id: 0,
                prog_if: 0,
                class: 2,
                subclass: 0,
            },
            command: Command::empty(),
            status: Status::empty(),
            cache_line_size: 0,
            latency_timer: 0,
            multifunction: false,
            bist_capable: false,
            bars: [None; 6],
            kind: DeviceKind::Device(DeviceDetails {
                cardbus_cis_ptr: 0,
                subsystem_vendor_id: 0,
                subsystem_id: 0,
                expansion_rom_base_addr: 0,
                min_grant: 0,
                max_latency: 0,
            }),
            pic_interrupt_line: 0,
            interrupt_pin: None,
            cspace_access_method: CSpaceAccessMethod::IO,
            capabilities: None,
        }
    }

    fn probe(dev: &PCIDevice) -> DeviceResult<Device> {
        probe_pci_device(dev, &None, None)
    }

    #[test]
    fn the_first_driver_that_matches_brings_the_device_up() {
        alone_with_the_registry(|| {
            let (first, first_calls) = fake("first", (0x8086, 0x15fa), Answer::Works);
            let (second, second_calls) = fake("second", (0x8086, 0x15fa), Answer::Works);
            replace_drivers(alloc::vec![first, second]);
            assert!(probe(&device(0x8086, 0x15fa)).is_ok());
            assert_eq!(first_calls.load(Ordering::SeqCst), 1);
            assert_eq!(
                second_calls.load(Ordering::SeqCst),
                0,
                "the second driver was asked for a device the first one took"
            );
        });
    }

    #[test]
    fn a_driver_that_does_not_match_is_never_initialised() {
        alone_with_the_registry(|| {
            let (drv, calls) = fake("nvme", (0x8086, 0xf1a5), Answer::Works);
            replace_drivers(alloc::vec![drv]);
            assert_eq!(
                probe(&device(0x10ec, 0x8168)).err(),
                Some(DeviceError::NotSupported)
            );
            assert_eq!(calls.load(Ordering::SeqCst), 0);
        });
    }

    #[test]
    fn a_driver_that_declines_lets_the_next_one_try() {
        alone_with_the_registry(|| {
            // The NVIDIA HDMI audio function on a GPU with no monitor: matched
            // the class, declined this function.
            let (declines, declined) = fake("hda", (0x10de, 0x1f08), Answer::Declines);
            let (works, worked) = fake("nvidia", (0x10de, 0x1f08), Answer::Works);
            replace_drivers(alloc::vec![declines, works]);
            assert!(probe(&device(0x10de, 0x1f08)).is_ok());
            assert_eq!(declined.load(Ordering::SeqCst), 1);
            assert_eq!(worked.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn a_driver_that_fails_for_real_is_not_reported_as_an_unknown_device() {
        // This is the one the caller acts on: `init_driver` treats the error as
        // "no modular driver for this" and walks on to the legacy path, so a
        // disk whose driver could not get DMA memory looked exactly like a disk
        // nobody has a driver for.
        alone_with_the_registry(|| {
            let (drv, calls) = fake(
                "nvme",
                (0x8086, 0xf1a5),
                Answer::Fails(DeviceError::DmaError),
            );
            replace_drivers(alloc::vec![drv]);
            assert_eq!(
                probe(&device(0x8086, 0xf1a5)).err(),
                Some(DeviceError::DmaError)
            );
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn the_first_real_failure_is_the_one_reported() {
        alone_with_the_registry(|| {
            let (first, _) = fake("first", (1, 1), Answer::Fails(DeviceError::DmaError));
            let (declines, _) = fake("skip", (1, 1), Answer::Declines);
            let (second, _) = fake("second", (1, 1), Answer::Fails(DeviceError::IoError));
            replace_drivers(alloc::vec![first, declines, second]);
            assert_eq!(probe(&device(1, 1)).err(), Some(DeviceError::DmaError));
        });
    }

    #[test]
    fn a_real_failure_does_not_stop_a_later_driver_from_winning() {
        alone_with_the_registry(|| {
            let (fails, failed) = fake("fails", (1, 1), Answer::Fails(DeviceError::IoError));
            let (works, worked) = fake("works", (1, 1), Answer::Works);
            replace_drivers(alloc::vec![fails, works]);
            assert!(probe(&device(1, 1)).is_ok());
            assert_eq!(failed.load(Ordering::SeqCst), 1);
            assert_eq!(worked.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn an_empty_registry_says_not_supported() {
        alone_with_the_registry(|| {
            replace_drivers(Vec::new());
            assert_eq!(probe(&device(1, 1)).err(), Some(DeviceError::NotSupported));
        });
    }

    #[test]
    fn the_registry_is_not_locked_while_a_driver_initialises() {
        // `init` is arbitrary driver code, and `DRIVERS` is a spin lock: with
        // the lock held across the call, a driver that registers a sub-driver
        // or probes a nested device hangs the boot and prints nothing.
        alone_with_the_registry(|| {
            let (drv, calls) = fake("nested", (1, 1), Answer::TouchesTheRegistry);
            replace_drivers(alloc::vec![drv]);
            assert!(
                probe(&device(1, 1)).is_ok(),
                "the driver could not reach the registry from its own init"
            );
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn a_driver_can_decide_by_class_instead_of_by_ids() {
        alone_with_the_registry(|| {
            let calls = counter();
            let drv = alloc::boxed::Box::leak(alloc::boxed::Box::new(ByClass { class: 2, calls }));
            replace_drivers(alloc::vec![drv as &'static (dyn PciDriver + Send + Sync)]);
            // `device()` builds class 2 (network) with ids this driver refuses.
            assert!(probe(&device(0xdead, 0xbeef)).is_ok());
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn the_registration_can_only_be_claimed_once() {
        alone_with_the_registry(|| {
            assert!(claim_registration());
            assert!(!claim_registration());
            assert!(!claim_registration());
        });
    }

    #[test]
    fn a_driver_in_the_registry_twice_is_asked_twice() {
        // Which is what a second `init_all` used to cause, and why the flag is
        // there: with a driver that works, the first `Device` --- its queues,
        // its interrupt handler, its DMA --- is built and then dropped on the
        // floor while the hardware is brought up again underneath it.
        alone_with_the_registry(|| {
            let (drv, calls) = fake("twice", (1, 1), Answer::Fails(DeviceError::IoError));
            replace_drivers(alloc::vec![drv, drv]);
            assert!(probe(&device(1, 1)).is_err());
            assert_eq!(calls.load(Ordering::SeqCst), 2);
        });
    }

    #[test]
    fn registering_all_the_drivers_twice_does_not_duplicate_them() {
        // A second `init_all` used to push every driver again, and then every
        // device was offered to the same driver twice: the second bring-up runs
        // against hardware the first one already took, and its queues and
        // interrupt handlers are the ones the caller keeps.
        alone_with_the_registry(|| {
            replace_drivers(Vec::new());
            init_all();
            let after_one = DRIVERS.lock().len();
            assert!(after_one > 0, "init_all registered nothing at all");
            init_all();
            assert_eq!(DRIVERS.lock().len(), after_one);
        });
    }
}
