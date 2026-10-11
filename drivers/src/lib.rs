//! Device drivers of zCore.

#![cfg_attr(not(feature = "mock"), no_std)]
#![deny(warnings)]
// The native `#[bench]` harness, asked for only in a test build. The rows
// live in `#[cfg(test)] mod benches` next to the code they measure, the same
// shape `linux-syscall` and `kernel-hal` use, because the interesting helpers
// here are private to their module. See `docs/README-benchmarks.md`.
#![cfg_attr(test, feature(test))]
// The pinned nightly's clippy flags a batch of style lints across the legacy
// driver code (register-offset tables written `0x0000/4` for column alignment,
// MMIO `transmute`s, wide hardware-init signatures, driver doc formatting).
// They are intentional low-level-driver patterns, not defects; blanket-allowing
// them here keeps `deny(warnings)` meaningful for real issues without churning
// vendored-style driver internals.
#![allow(clippy::erasing_op)] // `0x0000 / 4` register offset kept for alignment
#![allow(clippy::manual_clamp)] // explicit min/max on input deltas reads clearer
#![allow(clippy::needless_range_loop)] // index-parallel hardware descriptor walks
#![allow(clippy::missing_safety_doc)] // legacy unsafe MMIO helpers
#![allow(clippy::missing_transmute_annotations)] // MMIO register transmutes
#![allow(clippy::type_complexity)] // driver callback/handler tuples
#![allow(clippy::too_many_arguments)] // hardware bring-up entry points
#![allow(clippy::doc_lazy_continuation)] // driver doc-comment wrapping
#![feature(doc_cfg)]

extern crate alloc;
#[cfg(test)]
extern crate test;
// A test build links `std` anyway -- `test` depends on it -- and the
// non-temporal-blit instrumentation in `utils::dma_sync::test_flag` needs
// `thread_local!` to stay honest under a parallel harness: a process-global
// counter cannot say whether THIS test's blit took the fast path when another
// thread's blit can bump it in between.
#[cfg(test)]
extern crate std;

#[macro_use]
extern crate log;

use alloc::sync::Arc;
use core::fmt;

// Do not use `cfg(doc)` here: rustdoc sets it for `--doc` tests, which would
// compile these modules without their optional deps (`async-std`/`sdl2` /
// `virtio-drivers`) and without the `mock` feature that disables `no_std`.
#[cfg(feature = "mock")]
#[doc(cfg(feature = "mock"))]
pub mod mock;

#[cfg(feature = "virtio")]
#[doc(cfg(feature = "virtio"))]
pub mod virtio;

pub mod ata;
pub mod audio;
pub mod builder;
#[macro_use]
pub mod bus;
pub mod display;
pub mod hotplug;
pub mod input;
pub mod io;
pub mod irq;
pub mod net;
pub mod nvme;
pub mod prelude;
pub mod scheme;
pub mod sync;
pub mod uart;
pub mod utils;

#[cfg(all(
    any(feature = "xhci-usb-hid", feature = "legacy-usb-hid"),
    target_arch = "x86_64",
    not(feature = "mock"),
    not(feature = "no-pci")
))]
pub mod usb;

/// Initialize all drivers.
pub fn init() {
    #[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
    bus::pci_drivers::init_all();
}

/// The error type for external device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceError {
    /// The buffer is too small.
    BufferTooSmall,
    /// The device is not ready.
    NotReady,
    /// Invalid parameter.
    InvalidParam,
    /// Failed to alloc DMA memory.
    DmaError,
    /// I/O Error
    IoError,
    /// A resource with the specified identifier already exists.
    AlreadyExists,
    /// No resource to allocate.
    NoResources,
    /// The device driver is not implemented, supported, or enabled.
    NotSupported,
}

/// A type alias for the result of a device operation.
pub type DeviceResult<T = ()> = core::result::Result<T, DeviceError>;

/// Static shell of shared dynamic device [`Scheme`](crate::scheme::Scheme) types.
#[derive(Clone)]
pub enum Device {
    /// Block device
    Block(Arc<dyn scheme::BlockScheme>),
    /// Display device
    Display(Arc<dyn scheme::DisplayScheme>),
    /// Input device
    Input(Arc<dyn scheme::InputScheme>),
    /// Interrupt request and handle
    Irq(Arc<dyn scheme::IrqScheme>),
    /// Network device
    Net(Arc<dyn scheme::NetScheme>),
    /// Uart port
    Uart(Arc<dyn scheme::UartScheme>),
    /// DRM device
    Drm(Arc<dyn scheme::DrmScheme>),
    /// A device that acts as both a DRM device and a framebuffer display
    /// (e.g. NvidiaGpu).  Registered into both the DRM and display lists.
    DrmDisplay(Arc<dyn scheme::DrmScheme>, Arc<dyn scheme::DisplayScheme>),
    /// PCM audio output device
    Audio(Arc<dyn scheme::AudioScheme>),
}

impl Device {
    /// Get a general [`Scheme`](scheme::Scheme) from the device.
    pub fn inner(&self) -> Arc<dyn scheme::Scheme> {
        match self {
            Self::Block(d) => d.clone().upcast(),
            Self::Display(d) => d.clone().upcast(),
            Self::Input(d) => d.clone().upcast(),
            Self::Irq(d) => d.clone().upcast(),
            Self::Net(d) => d.clone().upcast(),
            Self::Uart(d) => d.clone().upcast(),
            Self::Drm(d) => d.clone().upcast(),
            Self::DrmDisplay(d, _) => d.clone().upcast(),
            Self::Audio(d) => d.clone().upcast(),
        }
    }
}

impl fmt::Debug for Device {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Block(d) => write!(f, "BlockDevice({:?})", d.name()),
            Self::Display(d) => write!(f, "DisplayDevice({:?})", d.name()),
            Self::Input(d) => write!(f, "InputDevice({:?})", d.name()),
            Self::Irq(d) => write!(f, "IrqDevice({:?})", d.name()),
            Self::Net(d) => write!(f, "NetDevice({:?})", d.name()),
            Self::Uart(d) => write!(f, "UartDevice({:?})", d.name()),
            Self::Drm(d) => write!(f, "DrmDevice({:?})", d.name()),
            Self::DrmDisplay(d, _) => write!(f, "DrmDisplayDevice({:?})", d.name()),
            Self::Audio(d) => write!(f, "AudioDevice({:?})", d.name()),
        }
    }
}

type PhysAddr = usize;
type VirtAddr = usize;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheme::impl_event_scheme;
    use crate::scheme::{
        AudioScheme, BlockScheme, DisplayScheme, DrmScheme, InputScheme, IrqScheme, NetScheme,
        Scheme, UartScheme,
    };
    use alloc::format;
    use alloc::string::String;
    use alloc::vec::Vec;

    /// One stub per `Device` variant, each with its own name, so a `Debug`
    /// line or an `inner()` that reached the wrong field is visible rather
    /// than merely equal.
    macro_rules! stub {
        ($name:ident, $text:literal) => {
            struct $name;
            impl Scheme for $name {
                fn name(&self) -> &str {
                    $text
                }
            }
        };
    }

    stub!(StubBlock, "stub-block");
    stub!(StubIrq, "stub-irq");
    stub!(StubNet, "stub-net");
    stub!(StubDrm, "stub-drm");
    stub!(StubAudio, "stub-audio");

    /// The display and input/uart stubs need state of their own: a
    /// framebuffer to hand out, and an event listener for the macro to
    /// forward to.
    struct StubDisplay {
        mem: crate::sync::Mutex<Vec<u8>>,
    }

    impl StubDisplay {
        fn new() -> Self {
            Self {
                mem: crate::sync::Mutex::new(alloc::vec![0u8; 4]),
            }
        }
    }

    impl Scheme for StubDisplay {
        fn name(&self) -> &str {
            "stub-display"
        }
    }

    struct StubInput {
        listener: crate::utils::EventListener<crate::scheme::input::InputEvent>,
    }

    impl StubInput {
        fn new() -> Self {
            Self {
                listener: crate::utils::EventListener::new(),
            }
        }
    }

    impl Scheme for StubInput {
        fn name(&self) -> &str {
            "stub-input"
        }
    }

    impl_event_scheme!(StubInput, crate::scheme::input::InputEvent);

    struct StubUart {
        listener: crate::utils::EventListener,
    }

    impl StubUart {
        fn new() -> Self {
            Self {
                listener: crate::utils::EventListener::new(),
            }
        }
    }

    impl Scheme for StubUart {
        fn name(&self) -> &str {
            "stub-uart"
        }
    }

    impl BlockScheme for StubBlock {
        fn read_block(&self, _id: usize, _buf: &mut [u8]) -> DeviceResult {
            Ok(())
        }
        fn write_block(&self, _id: usize, _buf: &[u8]) -> DeviceResult {
            Ok(())
        }
        fn flush(&self) -> DeviceResult {
            Ok(())
        }
        fn block_count(&self) -> usize {
            0
        }
    }

    impl DisplayScheme for StubDisplay {
        fn info(&self) -> crate::scheme::display::DisplayInfo {
            crate::scheme::display::DisplayInfo {
                width: 1,
                height: 1,
                pitch: 4,
                format: crate::scheme::display::ColorFormat::ARGB8888,
                fb_base_vaddr: 0,
                fb_size: 4,
            }
        }
        fn fb(&self) -> crate::scheme::display::FrameBuffer<'_> {
            let mut m = self.mem.lock();
            // SAFETY: the `Vec` is owned by `self` and outlives the view, the
            // same shape the real backends use for their aperture pointer.
            unsafe {
                crate::scheme::display::FrameBuffer::from_raw_parts_mut(m.as_mut_ptr(), m.len())
            }
        }
    }

    impl InputScheme for StubInput {
        fn capability(
            &self,
            _cap_type: crate::scheme::input::CapabilityType,
        ) -> crate::scheme::input::InputCapability {
            crate::scheme::input::InputCapability::empty()
        }
    }

    impl IrqScheme for StubIrq {
        fn is_valid_irq(&self, _irq_num: usize) -> bool {
            false
        }
        fn mask(&self, _irq_num: usize) -> DeviceResult {
            Ok(())
        }
        fn unmask(&self, _irq_num: usize) -> DeviceResult {
            Ok(())
        }
        fn register_handler(
            &self,
            _irq_num: usize,
            _handler: crate::scheme::IrqHandler,
        ) -> DeviceResult {
            Ok(())
        }
        fn unregister(&self, _irq_num: usize) -> DeviceResult {
            Ok(())
        }
    }

    impl NetScheme for StubNet {
        fn recv(&self, _buf: &mut [u8]) -> DeviceResult<usize> {
            Ok(0)
        }
        fn send(&self, _buf: &[u8]) -> DeviceResult<usize> {
            Ok(0)
        }
        fn get_mac(&self) -> smoltcp::wire::EthernetAddress {
            smoltcp::wire::EthernetAddress([0; 6])
        }
        fn get_ifname(&self) -> String {
            String::from("stub0")
        }
        fn get_ip_address(&self) -> Vec<smoltcp::wire::IpCidr> {
            Vec::new()
        }
        fn poll(&self) -> DeviceResult {
            Ok(())
        }
    }

    impl_event_scheme!(StubUart);

    impl UartScheme for StubUart {
        fn try_recv(&self) -> DeviceResult<Option<u8>> {
            Ok(None)
        }
        fn send(&self, _ch: u8) -> DeviceResult {
            Ok(())
        }
    }

    impl DrmScheme for StubDrm {
        fn get_caps(&self) -> crate::scheme::drm::DrmCaps {
            crate::scheme::drm::DrmCaps {
                has_3d: false,
                has_cursor: false,
                max_width: 0,
                max_height: 0,
            }
        }
        fn create_fb(&self, _h: u32, _w: u32, _ht: u32, _p: u32) -> Option<u32> {
            None
        }
        fn page_flip(&self, _fb_id: u32) -> bool {
            false
        }
        fn set_cursor(&self, _c: u32, _x: i32, _y: i32, _h: u32, _f: u32) -> bool {
            false
        }
        fn wait_vblank(&self, _crtc_id: u32) -> bool {
            false
        }
        fn get_resources(&self) -> (Vec<u32>, Vec<u32>, Vec<u32>) {
            (Vec::new(), Vec::new(), Vec::new())
        }
        fn get_connector(&self, _id: u32) -> Option<crate::scheme::drm::DrmConnector> {
            None
        }
        fn get_crtc(&self, _id: u32) -> Option<crate::scheme::drm::DrmCrtc> {
            None
        }
        fn get_plane(&self, _id: u32) -> Option<crate::scheme::drm::DrmPlane> {
            None
        }
        fn get_planes(&self) -> Vec<u32> {
            Vec::new()
        }
        fn set_plane(
            &self,
            _plane_id: u32,
            _crtc_id: u32,
            _fb_id: u32,
            _x: i32,
            _y: i32,
            _w: u32,
            _h: u32,
            _src_x: u32,
            _src_y: u32,
            _src_w: u32,
            _src_h: u32,
        ) -> bool {
            false
        }
    }

    impl AudioScheme for StubAudio {
        fn set_params(&self, rate: u32, channels: u8) -> DeviceResult<(u32, u8)> {
            Ok((rate, channels))
        }
        fn params(&self) -> (u32, u8) {
            (48_000, 2)
        }
        fn write(&self, pcm: &[u8]) -> DeviceResult<usize> {
            Ok(pcm.len())
        }
        fn free_bytes(&self) -> usize {
            0
        }
        fn buffer_bytes(&self) -> usize {
            0
        }
        fn queued_bytes(&self) -> usize {
            0
        }
        fn reset(&self) -> DeviceResult {
            Ok(())
        }
    }

    /// Every variant, each paired with the name its driver reports and the
    /// prefix `Debug` must print for it.
    fn one_of_each() -> Vec<(Device, &'static str, &'static str)> {
        alloc::vec![
            (
                Device::Block(Arc::new(StubBlock)),
                "stub-block",
                "BlockDevice"
            ),
            (
                Device::Display(Arc::new(StubDisplay::new())),
                "stub-display",
                "DisplayDevice"
            ),
            (
                Device::Input(Arc::new(StubInput::new())),
                "stub-input",
                "InputDevice"
            ),
            (Device::Irq(Arc::new(StubIrq)), "stub-irq", "IrqDevice"),
            (Device::Net(Arc::new(StubNet)), "stub-net", "NetDevice"),
            (Device::Uart(Arc::new(StubUart::new())), "stub-uart", "UartDevice"),
            (Device::Drm(Arc::new(StubDrm)), "stub-drm", "DrmDevice"),
            (
                Device::DrmDisplay(Arc::new(StubDrm), Arc::new(StubDisplay::new())),
                "stub-drm",
                "DrmDisplayDevice"
            ),
            (
                Device::Audio(Arc::new(StubAudio)),
                "stub-audio",
                "AudioDevice"
            ),
        ]
    }

    /// `inner()` is how `kernel-hal` gets at a device's interrupt handler
    /// once it has been put in the enum, so every variant must reach the
    /// driver it was built with -- including `DrmDisplay`, which holds two
    /// and must answer with the DRM one.
    #[test]
    fn every_variant_reaches_its_own_driver() {
        for (dev, name, _) in one_of_each() {
            assert_eq!(dev.inner().name(), name, "{:?} reached another driver", dev);
        }
    }

    /// The `Debug` line is what a bring-up log shows, so each variant must
    /// print under its own label and carry the driver's name: a mislabelled
    /// line sends whoever is reading the boot log to the wrong driver.
    #[test]
    fn every_variant_prints_under_its_own_label() {
        for (dev, name, label) in one_of_each() {
            let text = format!("{:?}", dev);
            assert!(text.starts_with(label), "{:?} is not a {}", text, label);
            assert!(text.contains(name), "{:?} does not name its driver", text);
        }
    }

    /// A `Device` is cloned into each of `kernel-hal`'s lists, and the clone
    /// must share the driver rather than duplicate it -- a DRM display is
    /// registered into BOTH the DRM and the display list, and two copies of a
    /// GPU's state would be two sets of registers.
    #[test]
    fn cloning_a_device_shares_its_driver() {
        let driver = Arc::new(StubDrm);
        let dev = Device::Drm(driver.clone());
        let before = Arc::strong_count(&driver);
        let copy = dev.clone();
        assert_eq!(Arc::strong_count(&driver), before + 1);
        assert_eq!(copy.inner().name(), "stub-drm");
        drop(copy);
        assert_eq!(Arc::strong_count(&driver), before);
    }

    /// `DrmDisplay` is the one variant holding two drivers; both must be the
    /// ones it was built with, which `inner()` alone cannot show.
    #[test]
    fn a_drm_display_keeps_both_of_its_drivers() {
        let drm = Arc::new(StubDrm);
        let display = Arc::new(StubDisplay::new());
        let dev = Device::DrmDisplay(drm.clone(), display.clone());
        match &dev {
            Device::DrmDisplay(d, s) => {
                assert_eq!(d.name(), "stub-drm");
                assert_eq!(s.name(), "stub-display");
            }
            other => panic!("built a DrmDisplay and got {:?}", other),
        }
    }

    /// `DeviceError` is compared by value all over the drivers (`== NotReady`
    /// decides whether a caller retries), so the variants must stay distinct
    /// under both equality and `Debug`.
    #[test]
    fn the_device_errors_are_distinct() {
        let all = [
            DeviceError::BufferTooSmall,
            DeviceError::NotReady,
            DeviceError::InvalidParam,
            DeviceError::DmaError,
            DeviceError::IoError,
            DeviceError::AlreadyExists,
            DeviceError::NoResources,
            DeviceError::NotSupported,
        ];
        for (i, a) in all.iter().enumerate() {
            for (j, b) in all.iter().enumerate() {
                if i == j {
                    assert_eq!(a, b);
                } else {
                    assert_ne!(a, b, "two errors compare equal");
                    assert_ne!(
                        format!("{:?}", a),
                        format!("{:?}", b),
                        "two errors print the same"
                    );
                }
            }
        }
    }

    /// `DeviceResult` defaults its payload to `()`, which is what lets the
    /// whole crate write `-> DeviceResult`; a change there would be a
    /// crate-wide signature change.
    #[test]
    fn a_device_result_without_a_payload_carries_the_unit() {
        let ok: DeviceResult = Ok(());
        assert_eq!(ok, Ok(()));
        let err: DeviceResult<usize> = Err(DeviceError::NotSupported);
        assert_eq!(err, Err(DeviceError::NotSupported));
    }
}
