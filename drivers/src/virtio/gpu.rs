use crate::sync::Mutex;
use alloc::vec::Vec;
use virtio_drivers::{VirtIOGpu as InnerDriver, VirtIOHeader};

use crate::prelude::{AccelCaps, ColorFormat, DisplayInfo, FrameBuffer};
use crate::scheme::{DisplayScheme, DrmScheme, Scheme};
use crate::{DeviceError, DeviceResult};

/// Device status bits, spec 1.1 §2.1.
const STATUS_ACKNOWLEDGE: u8 = 1;
const STATUS_DRIVER: u8 = 2;
const STATUS_DRIVER_OK: u8 = 4;
const STATUS_FEATURES_OK: u8 = 8;

/// The values a driver writes to `device_status` to bring a modern device up:
/// a reset first, then one bit added per step, `DRIVER_OK` last (spec 1.1 §3.1.1).
///
/// This is a list, and not the five `|=` statements it replaces, because five
/// plain read-modify-writes of one MMIO field are five loads and stores of the
/// same address where only the last value is observable: the compiler is
/// entitled to fold them into a single store of `0x0f`, and does. The device
/// then never sees the reset --- it comes up in whatever state the last boot
/// left it in --- and never sees `FEATURES_OK` before `DRIVER_OK`. Written one
/// `write_volatile` per element, the sequence survives the optimiser.
const fn status_sequence() -> [u8; 5] {
    [
        0,
        STATUS_ACKNOWLEDGE,
        STATUS_ACKNOWLEDGE | STATUS_DRIVER,
        STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK,
        STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK,
    ]
}

pub struct VirtIoGpu<'a> {
    info: DisplayInfo,
    inner: Option<Mutex<InnerDriver<'a>>>,
}

impl<'a> VirtIoGpu<'a> {
    pub fn new(header: &'static mut VirtIOHeader) -> DeviceResult<Self> {
        let mut gpu = InnerDriver::new(header)?;
        let fb = gpu.setup_framebuffer()?;
        let fb_base_vaddr = fb.as_ptr() as usize;
        let fb_size = fb.len();
        let (width, height) = gpu.resolution();
        let info = DisplayInfo {
            width,
            height,
            pitch: width * 4,
            format: ColorFormat::ARGB8888,
            fb_base_vaddr,
            fb_size,
        };
        Ok(Self {
            info,
            inner: Some(Mutex::new(gpu)),
        })
    }

    /// Initialize a VirtIO GPU in Modern mode (PCI)
    ///
    /// This path drives no virtqueue (`inner` stays `None`), so it cannot ask
    /// the device what it is displaying: the mode below is the one QEMU's
    /// VGA-compatible virtio device comes up in. What it can do is refuse to
    /// report a framebuffer that is not there, because what it reports is what
    /// the framebuffer console, `/dev/fb0` and the compositor write into.
    pub fn new_modern(
        common_vaddr: usize,
        _device_vaddr: usize,
        _notify_vaddr: usize,
        fb_vaddr: usize,
        fb_size: usize,
    ) -> DeviceResult<Self> {
        if common_vaddr == 0 {
            warn!("[virtio-gpu] modern bring-up without a common configuration window");
            return Err(DeviceError::InvalidParam);
        }

        let info = DisplayInfo {
            width: 1024,
            height: 768,
            pitch: 1024 * 4,
            format: ColorFormat::ARGB8888,
            fb_base_vaddr: fb_vaddr,
            // What the BAR holds, never a number of our own: `fb_size > 0 ?
            // fb_size : 1024 * 768 * 4` handed out three megabytes of
            // framebuffer at virtual address zero whenever the device had no
            // memory BAR0, and whoever cleared the screen wrote them.
            fb_size,
        };
        let needed = info.pitch as usize * info.height as usize;
        if fb_vaddr == 0 || fb_size < needed {
            warn!(
                "[virtio-gpu] modern bring-up has {:#x} bytes of framebuffer at {:#x}, and {}x{} needs {:#x}",
                fb_size, fb_vaddr, info.width, info.height, needed
            );
            return Err(DeviceError::InvalidParam);
        }

        // `addr_of_mut!` through the raw pointer: the field's offset in
        // `VirtioPciCommonCfg` *is* the register's address, and no reference to
        // device memory is created on the way.
        let status = unsafe {
            core::ptr::addr_of_mut!((*(common_vaddr as *mut VirtioPciCommonCfg)).device_status)
        };
        for step in status_sequence() {
            unsafe { status.write_volatile(step) };
        }
        let settled = unsafe { status.read_volatile() };
        if settled & STATUS_FEATURES_OK == 0 {
            // The device is allowed to clear FEATURES_OK to say it cannot work
            // with what was offered. Nothing is offered here, so this means the
            // device is unusable rather than fussy.
            warn!(
                "[virtio-gpu] device cleared FEATURES_OK (status {:#x}) during bring-up",
                settled
            );
        }

        // In Modern mode, we don't use the legacy InnerDriver because it requires a legacy header.
        Ok(Self { info, inner: None })
    }
}

#[repr(C)]
struct VirtioPciCommonCfg {
    device_feature_select: u32,
    device_feature: u32,
    driver_feature_select: u32,
    driver_feature: u32,
    msix_config: u16,
    num_queues: u16,
    device_status: u8,
    config_generation: u8,
    queue_select: u16,
    queue_size: u16,
    queue_msix_vector: u16,
    queue_enable: u16,
    queue_notify_off: u16,
    queue_desc: u64,
    queue_driver: u64,
    queue_device: u64,
}

impl<'a> Scheme for VirtIoGpu<'a> {
    fn name(&self) -> &str {
        "virtio-gpu"
    }

    fn handle_irq(&self, _irq_num: usize) {
        if let Some(inner) = &self.inner {
            inner.lock().ack_interrupt();
        }
    }
}

impl<'a> DisplayScheme for VirtIoGpu<'a> {
    fn info(&self) -> DisplayInfo {
        self.info
    }

    #[inline]
    fn fb(&self) -> FrameBuffer<'_> {
        unsafe {
            FrameBuffer::from_raw_parts_mut(self.info.fb_base_vaddr as *mut u8, self.info.fb_size)
        }
    }

    /// Host-shared RAM, not a WC BAR. Keep the scalar blit so QEMU
    /// software-KMS (and VirtualBox) stay cache-friendly.
    #[inline]
    fn fb_write_combining(&self) -> bool {
        false
    }

    /// The framebuffer is host-shared memory: the generic 2D primitives fill /
    /// copy / blit it in bulk in RAM and a single [`flush`](Self::flush) hands
    /// the dirty frame to the host (QEMU / VirtualBox) for display.
    fn accel_caps(&self) -> AccelCaps {
        AccelCaps {
            fill: true,
            copy: true,
            blit: true,
        }
    }

    fn need_flush(&self) -> bool {
        self.inner.is_some()
    }

    fn flush(&self) -> DeviceResult {
        if let Some(inner) = &self.inner {
            inner.lock().flush()?;
        }
        Ok(())
    }
}

use crate::scheme::drm::{DrmCaps, DrmConnector, DrmCrtc, DrmPlane, GemHandle};

impl<'a> DrmScheme for VirtIoGpu<'a> {
    fn get_caps(&self) -> DrmCaps {
        DrmCaps {
            has_3d: true,
            has_cursor: true,
            max_width: self.info.width,
            max_height: self.info.height,
        }
    }

    fn import_buffer(&self, _handle: GemHandle) -> bool {
        true
    }

    fn free_buffer(&self, _handle: GemHandle) {}

    fn create_fb(&self, handle_id: u32, _width: u32, _height: u32, _pitch: u32) -> Option<u32> {
        Some(handle_id)
    }

    fn page_flip(&self, _fb_id: u32) -> bool {
        self.flush().is_ok()
    }

    fn set_cursor(&self, _crtc_id: u32, _x: i32, _y: i32, _handle: u32, _flags: u32) -> bool {
        true
    }

    fn wait_vblank(&self, _crtc_id: u32) -> bool {
        true
    }

    fn get_resources(&self) -> (Vec<u32>, Vec<u32>, Vec<u32>) {
        (Vec::new(), alloc::vec![2000], alloc::vec![1000])
    }

    fn get_connector(&self, id: u32) -> Option<DrmConnector> {
        if id == 1000 {
            // Compute physical dimensions from the display resolution at ~96 DPI
            // (25.4 mm/inch, 96 px/inch). Use .max(1) to ensure non-zero values
            // so compositors do not see "Physical size: 0x0" warnings.
            let info = self.info();
            let mm_width = (info.width * 254 / 960).max(1);
            let mm_height = (info.height * 254 / 960).max(1);
            Some(DrmConnector {
                id,
                connected: true,
                mm_width,
                mm_height,
                connector_type: 11,
            })
        } else {
            None
        }
    }

    fn get_crtc(&self, id: u32) -> Option<DrmCrtc> {
        if id == 2000 {
            Some(DrmCrtc {
                id,
                fb_id: 0,
                x: 0,
                y: 0,
            })
        } else {
            None
        }
    }

    fn get_plane(&self, id: u32) -> Option<DrmPlane> {
        if id == 3000 {
            Some(DrmPlane {
                id,
                crtc_id: 2000,
                fb_id: 0,
                possible_crtcs: 1,
                plane_type: 1,
            })
        } else {
            None
        }
    }

    fn get_planes(&self) -> Vec<u32> {
        alloc::vec![3000]
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
        true
    }

    fn ioctl(&self, request: u32, _arg: usize) -> Result<usize, i32> {
        const DRM_IOCTL_VIRTGPU_GETPARAM: u32 = 0xC0106443;
        match request {
            DRM_IOCTL_VIRTGPU_GETPARAM => Ok(0),
            _ => Err(38),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{align_of, size_of, MaybeUninit};
    use core::ptr::addr_of;

    /// The mode this path reports, in bytes.
    const MODE_BYTES: usize = 1024 * 768 * 4;

    /// Where `device_status` lives in the common configuration window
    /// (spec 1.1 §4.1.4.3). Pinned to the struct by
    /// [`the_common_cfg_offsets_are_the_ones_the_spec_fixes`].
    const STATUS_OFFSET: usize = 0x14;

    /// Sixty-four 8-aligned bytes standing in for the device's common
    /// configuration window.
    fn fake_common_cfg() -> usize {
        let cells: &'static mut [u64; 8] =
            alloc::boxed::Box::leak(alloc::boxed::Box::new([0u64; 8]));
        cells.as_mut_ptr() as usize
    }

    fn status_of(vaddr: usize) -> u8 {
        unsafe { ((vaddr + STATUS_OFFSET) as *const u8).read_volatile() }
    }

    /// A GPU brought up over a window we own, with a framebuffer address that
    /// is never dereferenced.
    fn gpu_up() -> VirtIoGpu<'static> {
        VirtIoGpu::new_modern(fake_common_cfg(), 0, 0, 0x1000_0000, MODE_BYTES).unwrap()
    }

    /// `#[repr(C)]` over device memory means a field's offset *is* the
    /// register's address. One field inserted or reordered and the bring-up
    /// writes `DRIVER_OK` into `queue_select` instead, which no test of
    /// behaviour would notice.
    #[test]
    fn the_common_cfg_offsets_are_the_ones_the_spec_fixes() {
        let uninit = MaybeUninit::<VirtioPciCommonCfg>::uninit();
        let p = uninit.as_ptr();
        let base = p as usize;
        let at = |field: usize| field - base;

        unsafe {
            assert_eq!(at(addr_of!((*p).device_feature_select) as usize), 0x00);
            assert_eq!(at(addr_of!((*p).device_feature) as usize), 0x04);
            assert_eq!(at(addr_of!((*p).driver_feature_select) as usize), 0x08);
            assert_eq!(at(addr_of!((*p).driver_feature) as usize), 0x0c);
            assert_eq!(at(addr_of!((*p).msix_config) as usize), 0x10);
            assert_eq!(at(addr_of!((*p).num_queues) as usize), 0x12);
            assert_eq!(at(addr_of!((*p).device_status) as usize), STATUS_OFFSET);
            assert_eq!(at(addr_of!((*p).config_generation) as usize), 0x15);
            assert_eq!(at(addr_of!((*p).queue_select) as usize), 0x16);
            assert_eq!(at(addr_of!((*p).queue_size) as usize), 0x18);
            assert_eq!(at(addr_of!((*p).queue_msix_vector) as usize), 0x1a);
            assert_eq!(at(addr_of!((*p).queue_enable) as usize), 0x1c);
            assert_eq!(at(addr_of!((*p).queue_notify_off) as usize), 0x1e);
            assert_eq!(at(addr_of!((*p).queue_desc) as usize), 0x20);
            assert_eq!(at(addr_of!((*p).queue_driver) as usize), 0x28);
            assert_eq!(at(addr_of!((*p).queue_device) as usize), 0x30);
        }
        assert_eq!(size_of::<VirtioPciCommonCfg>(), 56);
        assert_eq!(align_of::<VirtioPciCommonCfg>(), 8);
    }

    #[test]
    fn the_status_handshake_resets_first_and_ends_at_driver_ok() {
        let seq = status_sequence();
        assert_eq!(seq[0], 0, "the first write must be the reset");
        assert_eq!(
            seq[seq.len() - 1],
            STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK
        );
        let features_ok = seq.iter().position(|s| s & STATUS_FEATURES_OK != 0);
        let driver_ok = seq.iter().position(|s| s & STATUS_DRIVER_OK != 0);
        assert!(
            features_ok < driver_ok,
            "FEATURES_OK must be set before DRIVER_OK, got {:?}",
            seq
        );
        for pair in seq[1..].windows(2) {
            assert_eq!(
                pair[0] & pair[1],
                pair[0],
                "step {:#x} -> {:#x} drops a bit",
                pair[0],
                pair[1]
            );
            assert_ne!(pair[0], pair[1], "a step that changes nothing");
        }
    }

    #[test]
    fn a_modern_gpu_walks_the_handshake_over_the_window_it_was_given() {
        let cfg = fake_common_cfg();
        let gpu = VirtIoGpu::new_modern(cfg, 0, 0, 0x1000_0000, MODE_BYTES).unwrap();
        assert_eq!(
            status_of(cfg),
            STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK
        );
        assert!(!gpu.need_flush());
    }

    /// The size used to be `if fb_size > 0 { fb_size } else { 1024 * 768 * 4 }`,
    /// and the transport passes zero for address and size alike when the device
    /// has no memory BAR 0: that handed out three megabytes of framebuffer at
    /// virtual address zero, and whoever cleared the screen wrote them.
    #[test]
    fn a_framebuffer_the_device_does_not_have_is_not_invented() {
        let cfg = fake_common_cfg();
        assert!(VirtIoGpu::new_modern(cfg, 0, 0, 0, 0).is_err());
        assert_eq!(
            status_of(cfg),
            0,
            "a device we refuse must not be left half brought up"
        );
        // A device that reports BAR 0 at physical address zero gets the size
        // right and the address wrong, which is the half the size check cannot
        // see.
        assert!(VirtIoGpu::new_modern(cfg, 0, 0, 0, MODE_BYTES).is_err());
    }

    /// What these tests cannot see, and why.
    ///
    /// The handshake's *content* is pinned by
    /// [`the_status_handshake_resets_first_and_ends_at_driver_ok`] and the fact
    /// that it is written at all by
    /// [`a_modern_gpu_walks_the_handshake_over_the_window_it_was_given`]. That
    /// each step reaches the device as its own store is not testable here: the
    /// window under test is ordinary memory, so folding the five writes into
    /// one leaves exactly the same final byte. It is the reason the writes are
    /// `write_volatile` and the reason the sequence is a list.
    #[test]
    fn the_handshake_is_five_writes_and_a_fake_device_cannot_count_them() {
        assert_eq!(status_sequence().len(), 5);
    }

    #[test]
    fn a_framebuffer_smaller_than_the_mode_is_refused() {
        let cfg = fake_common_cfg();
        assert!(VirtIoGpu::new_modern(cfg, 0, 0, 0x1000_0000, MODE_BYTES - 1).is_err());
        assert!(VirtIoGpu::new_modern(cfg, 0, 0, 0x1000_0000, 0).is_err());
    }

    #[test]
    fn a_modern_gpu_without_a_common_window_is_refused() {
        assert!(VirtIoGpu::new_modern(0, 0, 0, 0x1000_0000, MODE_BYTES).is_err());
    }

    #[test]
    fn the_display_it_reports_fits_in_the_framebuffer_it_was_given() {
        let cfg = fake_common_cfg();
        let gpu = VirtIoGpu::new_modern(cfg, 0, 0, 0x1000_0000, 0x0100_0000).unwrap();
        let info = gpu.info();
        assert_eq!((info.width, info.height), (1024, 768));
        assert_eq!(info.pitch, info.width * 4);
        assert!(info.fb_size >= info.pitch as usize * info.height as usize);
        assert_eq!(
            info.fb_size, 0x0100_0000,
            "the size reported is the BAR's, not one of ours"
        );
        assert_eq!(info.fb_base_vaddr, 0x1000_0000);
    }

    #[test]
    fn a_modern_gpu_has_no_virtqueue_to_flush() {
        let gpu = gpu_up();
        assert!(!gpu.need_flush());
        assert!(gpu.flush().is_ok());
        assert!(!gpu.fb_write_combining());
        let caps = gpu.accel_caps();
        assert!(caps.fill && caps.copy && caps.blit);
        assert_eq!(gpu.name(), "virtio-gpu");
    }

    #[test]
    fn every_resource_it_lists_is_one_it_answers_for() {
        let gpu = gpu_up();
        let (fbs, crtcs, connectors) = gpu.get_resources();
        assert!(fbs.is_empty());
        assert!(!crtcs.is_empty() && !connectors.is_empty());
        for id in crtcs {
            assert!(
                gpu.get_crtc(id).is_some(),
                "crtc {} is listed and unknown",
                id
            );
        }
        for id in connectors {
            assert!(
                gpu.get_connector(id).is_some(),
                "connector {} is listed and unknown",
                id
            );
        }
        for id in gpu.get_planes() {
            let plane = gpu.get_plane(id).expect("a listed plane");
            assert!(
                gpu.get_crtc(plane.crtc_id).is_some(),
                "plane {} hangs off crtc {}, which is unknown",
                id,
                plane.crtc_id
            );
        }
        assert!(gpu.get_crtc(0).is_none());
        assert!(gpu.get_plane(0).is_none());
    }

    #[test]
    fn the_connector_reports_a_physical_size_at_96_dpi() {
        let gpu = gpu_up();
        let c = gpu.get_connector(1000).expect("the one connector");
        assert!(c.connected);
        assert_eq!(c.connector_type, 11);
        assert_eq!(
            (c.mm_width, c.mm_height),
            (1024 * 254 / 960, 768 * 254 / 960)
        );
        assert!(c.mm_width > 0 && c.mm_height > 0);
        assert!(gpu.get_connector(1001).is_none());
    }

    #[test]
    fn the_caps_it_reports_are_the_mode_it_reports() {
        let gpu = gpu_up();
        let caps = gpu.get_caps();
        let info = gpu.info();
        assert_eq!((caps.max_width, caps.max_height), (info.width, info.height));
        assert!(caps.has_cursor);
    }

    #[test]
    fn the_only_ioctl_it_knows_is_getparam() {
        let gpu = gpu_up();
        assert_eq!(gpu.ioctl(0xc010_6443, 0), Ok(0));
        assert_eq!(gpu.ioctl(0xc010_6444, 0), Err(38));
    }
}
