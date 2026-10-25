use crate::PAGE_SIZE;
use bitflags::*;
use volatile::{ReadOnly, Volatile, WriteOnly};

/// MMIO Device Legacy Register Interface.
///
/// Ref: 4.2.4 Legacy interface
#[repr(C)]
pub struct VirtIOHeader {
    /// Magic value
    magic: ReadOnly<u32>,

    /// Device version number
    ///
    /// Legacy device returns value 0x1.
    version: ReadOnly<u32>,

    /// Virtio Subsystem Device ID
    device_id: ReadOnly<u32>,

    /// Virtio Subsystem Vendor ID
    vendor_id: ReadOnly<u32>,

    /// Flags representing features the device supports
    device_features: ReadOnly<u32>,

    /// Device (host) features word selection
    device_features_sel: WriteOnly<u32>,

    /// Reserved
    __r1: [ReadOnly<u32>; 2],

    /// Flags representing device features understood and activated by the driver
    driver_features: WriteOnly<u32>,

    /// Activated (guest) features word selection
    driver_features_sel: WriteOnly<u32>,

    /// Guest page size
    ///
    /// The driver writes the guest page size in bytes to the register during
    /// initialization, before any queues are used. This value should be a
    /// power of 2 and is used by the device to calculate the Guest address
    /// of the first queue page (see QueuePFN).
    guest_page_size: WriteOnly<u32>,

    /// Reserved
    __r2: ReadOnly<u32>,

    /// Virtual queue index
    ///
    /// Writing to this register selects the virtual queue that the following
    /// operations on the QueueNumMax, QueueNum, QueueAlign and QueuePFN
    /// registers apply to. The index number of the first queue is zero (0x0).
    queue_sel: WriteOnly<u32>,

    /// Maximum virtual queue size
    ///
    /// Reading from the register returns the maximum size of the queue the
    /// device is ready to process or zero (0x0) if the queue is not available.
    /// This applies to the queue selected by writing to QueueSel and is
    /// allowed only when QueuePFN is set to zero (0x0), so when the queue is
    /// not actively used.
    queue_num_max: ReadOnly<u32>,

    /// Virtual queue size
    ///
    /// Queue size is the number of elements in the queue. Writing to this
    /// register notifies the device what size of the queue the driver will use.
    /// This applies to the queue selected by writing to QueueSel.
    queue_num: WriteOnly<u32>,

    /// Used Ring alignment in the virtual queue
    ///
    /// Writing to this register notifies the device about alignment boundary
    /// of the Used Ring in bytes. This value should be a power of 2 and
    /// applies to the queue selected by writing to QueueSel.
    queue_align: WriteOnly<u32>,

    /// Guest physical page number of the virtual queue
    ///
    /// Writing to this register notifies the device about location of the
    /// virtual queue in the Guest’s physical address space. This value is
    /// the index number of a page starting with the queue Descriptor Table.
    /// Value zero (0x0) means physical address zero (0x00000000) and is illegal.
    /// When the driver stops using the queue it writes zero (0x0) to this
    /// register. Reading from this register returns the currently used page
    /// number of the queue, therefore a value other than zero (0x0) means that
    /// the queue is in use. Both read and write accesses apply to the queue
    /// selected by writing to QueueSel.
    queue_pfn: Volatile<u32>,

    /// new interface only
    queue_ready: Volatile<u32>,

    /// Reserved
    __r3: [ReadOnly<u32>; 2],

    /// Queue notifier
    queue_notify: WriteOnly<u32>,

    /// Reserved
    __r4: [ReadOnly<u32>; 3],

    /// Interrupt status
    interrupt_status: ReadOnly<u32>,

    /// Interrupt acknowledge
    interrupt_ack: WriteOnly<u32>,

    /// Reserved
    __r5: [ReadOnly<u32>; 2],

    /// Device status
    ///
    /// Reading from this register returns the current device status flags.
    /// Writing non-zero values to this register sets the status flags,
    /// indicating the OS/driver progress. Writing zero (0x0) to this register
    /// triggers a device reset. The device sets QueuePFN to zero (0x0) for
    /// all queues in the device. Also see 3.1 Device Initialization.
    status: Volatile<DeviceStatus>,

    /// Reserved
    __r6: [ReadOnly<u32>; 3],

    // new interface only since here
    queue_desc_low: WriteOnly<u32>,
    queue_desc_high: WriteOnly<u32>,

    /// Reserved
    __r7: [ReadOnly<u32>; 2],

    queue_avail_low: WriteOnly<u32>,
    queue_avail_high: WriteOnly<u32>,

    /// Reserved
    __r8: [ReadOnly<u32>; 2],

    queue_used_low: WriteOnly<u32>,
    queue_used_high: WriteOnly<u32>,

    /// Reserved
    __r9: [ReadOnly<u32>; 21],

    config_generation: ReadOnly<u32>,
}

impl VirtIOHeader {
    /// Verify a valid header.
    pub fn verify(&self) -> bool {
        self.magic.read() == 0x7472_6976 && self.version.read() == 1 && self.device_id.read() != 0
    }

    /// Get the device type.
    ///
    /// The id comes off a device register, so it is not a `DeviceType` until
    /// something checks it. This used to `transmute` it after a range check --
    /// and the two had to agree exactly, because the enum has holes at 14 and
    /// 15: a range that let one of those through produced a `DeviceType` with
    /// no variant, which is undefined behaviour and, being undefined, could not
    /// be tested for either.
    pub fn device_type(&self) -> DeviceType {
        match self.device_id.read() {
            1 => DeviceType::Network,
            2 => DeviceType::Block,
            3 => DeviceType::Console,
            4 => DeviceType::EntropySource,
            5 => DeviceType::MemoryBallooning,
            6 => DeviceType::IoMemory,
            7 => DeviceType::Rpmsg,
            8 => DeviceType::ScsiHost,
            9 => DeviceType::_9P,
            10 => DeviceType::Mac80211,
            11 => DeviceType::RprocSerial,
            12 => DeviceType::VirtioCAIF,
            13 => DeviceType::MemoryBalloon,
            16 => DeviceType::GPU,
            17 => DeviceType::Timer,
            18 => DeviceType::Input,
            19 => DeviceType::Socket,
            20 => DeviceType::Crypto,
            21 => DeviceType::SignalDistributionModule,
            22 => DeviceType::Pstore,
            23 => DeviceType::IOMMU,
            24 => DeviceType::Memory,
            _ => DeviceType::Invalid,
        }
    }

    /// Get the vendor ID.
    pub fn vendor_id(&self) -> u32 {
        self.vendor_id.read()
    }

    /// Begin initializing the device.
    ///
    /// Ref: virtio 3.1.1 Device Initialization
    pub fn begin_init(&mut self, negotiate_features: impl FnOnce(u64) -> u64) {
        self.status.write(DeviceStatus::ACKNOWLEDGE);
        self.status.write(DeviceStatus::DRIVER);

        let features = self.read_device_features();
        self.write_driver_features(negotiate_features(features));
        self.status.write(DeviceStatus::FEATURES_OK);

        self.guest_page_size.write(PAGE_SIZE as u32);
    }

    /// Finish initializing the device.
    pub fn finish_init(&mut self) {
        self.status.write(DeviceStatus::DRIVER_OK);
    }

    /// Read device features.
    fn read_device_features(&mut self) -> u64 {
        self.device_features_sel.write(0); // device features [0, 32)
        let mut device_features_bits = self.device_features.read().into();
        self.device_features_sel.write(1); // device features [32, 64)
        device_features_bits += (self.device_features.read() as u64) << 32;
        device_features_bits
    }

    /// Write device features.
    fn write_driver_features(&mut self, driver_features: u64) {
        self.driver_features_sel.write(0); // driver features [0, 32)
        self.driver_features.write(driver_features as u32);
        self.driver_features_sel.write(1); // driver features [32, 64)
        self.driver_features.write((driver_features >> 32) as u32);
    }

    /// Set queue.
    pub fn queue_set(&mut self, queue: u32, size: u32, align: u32, pfn: u32) {
        self.queue_sel.write(queue);
        self.queue_num.write(size);
        self.queue_align.write(align);
        self.queue_pfn.write(pfn);
    }

    /// Get guest physical page number of the virtual queue.
    pub fn queue_physical_page_number(&mut self, queue: u32) -> u32 {
        self.queue_sel.write(queue);
        self.queue_pfn.read()
    }

    /// Whether the queue is in used.
    pub fn queue_used(&mut self, queue: u32) -> bool {
        self.queue_physical_page_number(queue) != 0
    }

    /// Get the max size of queue.
    pub fn max_queue_size(&self) -> u32 {
        self.queue_num_max.read()
    }

    /// Notify device.
    pub fn notify(&mut self, queue: u32) {
        self.queue_notify.write(queue);
    }

    /// Acknowledge interrupt and return true if success.
    pub fn ack_interrupt(&mut self) -> bool {
        let interrupt = self.interrupt_status.read();
        if interrupt != 0 {
            self.interrupt_ack.write(interrupt);
            true
        } else {
            false
        }
    }

    /// Get the pointer to config space (at offset 0x100)
    pub fn config_space(&self) -> *mut u64 {
        (self as *const _ as usize + CONFIG_SPACE_OFFSET) as _
    }

    /// Make a zeroed region answer like a device, for the host test suite.
    ///
    /// Only the registers a device drives itself. The ones the driver writes
    /// are left alone, so a test can read back what it wrote.
    #[cfg(test)]
    pub(crate) fn fake_init(&mut self, device_id: u32, max_queue_size: u32) {
        assert!(
            core::mem::size_of::<Self>() <= CONFIG_SPACE_OFFSET,
            "the header grew into its own config space"
        );
        self.magic = ReadOnly::new(0x7472_6976);
        self.version = ReadOnly::new(1);
        self.device_id = ReadOnly::new(device_id);
        self.vendor_id = ReadOnly::new(0x554d_4551);
        self.queue_num_max = ReadOnly::new(max_queue_size);
    }

    /// Read back a register the driver is only supposed to write.
    ///
    /// A `WriteOnly` cell has no reader, by design: a device would not offer
    /// one. This region is ordinary memory, so the test suite can look at what
    /// landed there, which is the only way to check the driver wrote it.
    #[cfg(test)]
    fn peek(cell: &WriteOnly<u32>) -> u32 {
        unsafe { (cell as *const WriteOnly<u32> as *const u32).read_volatile() }
    }

    /// What the driver last wrote to the status register.
    #[cfg(test)]
    pub(crate) fn fake_status(&self) -> u32 {
        self.status.read().bits()
    }

    /// The last half of the feature word the driver wrote.
    ///
    /// A device keeps a register per selector value; this fake keeps one cell,
    /// so what survives is whichever half went in last.
    #[cfg(test)]
    pub(crate) fn fake_driver_features_last(&self) -> u32 {
        Self::peek(&self.driver_features)
    }

    /// Which half of the feature word the driver last selected.
    #[cfg(test)]
    pub(crate) fn fake_driver_features_sel(&self) -> u32 {
        Self::peek(&self.driver_features_sel)
    }

    /// The page size the driver told the device to assume.
    #[cfg(test)]
    pub(crate) fn fake_guest_page_size(&self) -> u32 {
        Self::peek(&self.guest_page_size)
    }

    /// The queue size the driver last published, and its alignment.
    #[cfg(test)]
    pub(crate) fn fake_queue_size(&self) -> (u32, u32) {
        (Self::peek(&self.queue_num), Self::peek(&self.queue_align))
    }

    /// The queue the driver last notified.
    #[cfg(test)]
    pub(crate) fn fake_notified(&self) -> u32 {
        Self::peek(&self.queue_notify)
    }

    /// Forget the queue address the driver just published.
    ///
    /// A device keeps a `QueuePFN` per queue, behind the `QueueSel` selector.
    /// This fake is plain memory, so it keeps one, and a driver that builds a
    /// second queue would be told the first one's address and refuse with
    /// `AlreadyUsed`. A test that wants two queues calls this in between, which
    /// is what selecting an unused queue on a real device amounts to.
    #[cfg(test)]
    pub(crate) fn fake_forget_queue_pfn(&mut self) {
        self.queue_pfn.write(0);
    }

    /// Raise an interrupt, the way a device does.
    #[cfg(test)]
    pub(crate) fn fake_raise_interrupt(&mut self, status: u32) {
        self.interrupt_status = ReadOnly::new(status);
    }

    /// What the driver acknowledged.
    #[cfg(test)]
    pub(crate) fn fake_interrupt_ack(&self) -> u32 {
        Self::peek(&self.interrupt_ack)
    }
}

bitflags! {
    /// The device status field.
    struct DeviceStatus: u32 {
        /// Indicates that the guest OS has found the device and recognized it
        /// as a valid virtio device.
        const ACKNOWLEDGE = 1;

        /// Indicates that the guest OS knows how to drive the device.
        const DRIVER = 2;

        /// Indicates that something went wrong in the guest, and it has given
        /// up on the device. This could be an internal error, or the driver
        /// didn’t like the device for some reason, or even a fatal error
        /// during device operation.
        const FAILED = 128;

        /// Indicates that the driver has acknowledged all the features it
        /// understands, and feature negotiation is complete.
        const FEATURES_OK = 8;

        /// Indicates that the driver is set up and ready to drive the device.
        const DRIVER_OK = 4;

        /// Indicates that the device has experienced an error from which it
        /// can’t recover.
        const DEVICE_NEEDS_RESET = 64;
    }
}

const CONFIG_SPACE_OFFSET: usize = 0x100;

/// Types of virtio devices.
#[repr(u8)]
#[derive(Debug, Eq, PartialEq)]
#[allow(missing_docs)]
pub enum DeviceType {
    Invalid = 0,
    Network = 1,
    Block = 2,
    Console = 3,
    EntropySource = 4,
    MemoryBallooning = 5,
    IoMemory = 6,
    Rpmsg = 7,
    ScsiHost = 8,
    _9P = 9,
    Mac80211 = 10,
    RprocSerial = 11,
    VirtioCAIF = 12,
    MemoryBalloon = 13,
    GPU = 16,
    Timer = 17,
    Input = 18,
    Socket = 19,
    Crypto = 20,
    SignalDistributionModule = 21,
    Pstore = 22,
    IOMMU = 23,
    Memory = 24,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_dev::fake_header;

    #[test]
    fn a_window_with_the_magic_and_a_device_verifies() {
        let header = fake_header(2, 16);
        assert!(header.verify());
    }

    #[test]
    fn a_window_with_no_device_behind_it_does_not_verify() {
        // Device id 0 is "no device here": the slot exists in the device tree
        // and nothing is plugged into it.
        let header = fake_header(0, 16);
        assert!(!header.verify());
    }

    #[test]
    fn the_device_type_is_read_from_the_id() {
        for (id, expected) in [
            (1u32, DeviceType::Network),
            (2, DeviceType::Block),
            (3, DeviceType::Console),
            (16, DeviceType::GPU),
            (18, DeviceType::Input),
            (24, DeviceType::Memory),
        ] {
            assert_eq!(fake_header(id, 16).device_type(), expected, "id {}", id);
        }
    }

    /// And the whole table, every id the specification assigns, each one to a
    /// type of its own.
    ///
    /// The six above are a spot check; what this one adds is that no two ids
    /// land on the SAME variant. The table is twenty-five arms written out by
    /// hand, so the mistake it invites is not a wrong number -- that the spot
    /// check would catch -- but a copied line: `12 => RprocSerial` under
    /// `11 => RprocSerial`. The kernel picks a driver off this answer, so a
    /// duplicate hands a device to the driver for a different one, and the
    /// first thing that driver does is program registers that are not there.
    #[test]
    fn no_two_device_ids_answer_with_the_same_type() {
        let table = [
            (1u32, DeviceType::Network),
            (2, DeviceType::Block),
            (3, DeviceType::Console),
            (4, DeviceType::EntropySource),
            (5, DeviceType::MemoryBallooning),
            (6, DeviceType::IoMemory),
            (7, DeviceType::Rpmsg),
            (8, DeviceType::ScsiHost),
            (9, DeviceType::_9P),
            (10, DeviceType::Mac80211),
            (11, DeviceType::RprocSerial),
            (12, DeviceType::VirtioCAIF),
            (13, DeviceType::MemoryBalloon),
            (16, DeviceType::GPU),
            (17, DeviceType::Timer),
            (18, DeviceType::Input),
            (19, DeviceType::Socket),
            (20, DeviceType::Crypto),
            (21, DeviceType::SignalDistributionModule),
            (22, DeviceType::Pstore),
            (23, DeviceType::IOMMU),
            (24, DeviceType::Memory),
        ];
        let total = table.len();
        let mut seen: Vec<String> = Vec::new();
        for (id, expected) in table {
            let got = fake_header(id, 16).device_type();
            assert_eq!(got, expected, "id {}", id);
            let name = format!("{:?}", got);
            assert!(
                !seen.contains(&name),
                "id {} answers {:?}, which another id already answered",
                id,
                got
            );
            seen.push(name);
        }
        assert_eq!(seen.len(), total);
    }

    /// The vendor id is its own register, two words after the device id, and
    /// nothing else in the window reads back as it.
    ///
    /// It is what names the implementation in the boot log, so a wrong offset
    /// does not stop anything working -- it reports the device id, or the
    /// version, as the vendor, and the one place that would show is a log
    /// nobody reads twice. QEMU's is the four bytes of "QEMU".
    #[test]
    fn the_vendor_id_comes_off_its_own_register() {
        let header = fake_header(2, 16);

        assert_eq!(header.vendor_id(), 0x554d_4551, "not QEMU's vendor id");
        assert_eq!(
            &header.vendor_id().to_le_bytes(),
            b"QEMU",
            "the four bytes are not QEMU's, so the offset is wrong"
        );
        // And it is not any of its neighbours in the window.
        assert_ne!(header.vendor_id(), 2, "it is reading the device id");
        assert_ne!(header.vendor_id(), 1, "it is reading the version");
        assert_ne!(header.vendor_id(), 0x7472_6976, "it is reading the magic");
    }

    #[test]
    fn an_id_with_no_variant_is_invalid_rather_than_transmuted() {
        // `device_type` transmutes the id into the enum, so the gaps matter:
        // 14 and 15 have no variant, and neither does anything above 24.
        for id in [0u32, 14, 15, 25, 100, 0xffff_ffff] {
            assert_eq!(
                fake_header(id, 16).device_type(),
                DeviceType::Invalid,
                "id {}",
                id
            );
        }
    }

    #[test]
    fn the_driver_offers_both_halves_of_the_feature_word() {
        // The feature word is 64 bits behind a 32-bit register with a
        // selector, so the driver has to go round twice. A driver that wrote
        // only the low half would silently drop `VERSION_1` and everything
        // above it, which is every feature virtio 1.0 added.
        let header = fake_header(2, 16);
        header.begin_init(|offered| {
            assert_eq!(offered, 0, "the fake device offers nothing");
            0x3_0000_0005
        });
        assert_eq!(header.fake_driver_features_sel(), 1);
        assert_eq!(
            header.fake_driver_features_last(),
            0x3,
            "the top half of the feature word never reached the device"
        );
    }

    #[test]
    fn begin_init_states_the_page_size_the_layout_assumes() {
        // The device computes the address of the queue from `QueuePFN` and
        // this, so a driver that laid its rings out in 4 KiB pages and never
        // said so would be read at the wrong address.
        let header = fake_header(2, 16);
        header.begin_init(|_| 0);
        assert_eq!(header.fake_guest_page_size(), crate::PAGE_SIZE as u32);
    }

    #[test]
    fn finish_init_says_the_driver_is_ready() {
        let header = fake_header(2, 16);
        header.begin_init(|_| 0);
        header.finish_init();
        assert_eq!(
            header.fake_status() & DeviceStatus::DRIVER_OK.bits(),
            DeviceStatus::DRIVER_OK.bits()
        );
    }

    #[test]
    fn an_interrupt_nobody_raised_is_not_acknowledged() {
        let header = fake_header(2, 16);
        assert!(!header.ack_interrupt());
        assert_eq!(header.fake_interrupt_ack(), 0);
    }

    #[test]
    fn an_interrupt_is_acknowledged_with_the_bits_that_were_raised() {
        let header = fake_header(2, 16);
        header.fake_raise_interrupt(0b11);
        assert!(header.ack_interrupt());
        assert_eq!(header.fake_interrupt_ack(), 0b11);
    }

    #[test]
    fn a_queue_nobody_published_is_not_in_use() {
        let header = fake_header(2, 16);
        assert!(!header.queue_used(0));
    }

    #[test]
    fn queue_set_publishes_what_it_was_given() {
        let header = fake_header(2, 16);
        header.queue_set(0, 8, 4096, 0x1234);
        assert!(header.queue_used(0));
        assert_eq!(header.queue_physical_page_number(0), 0x1234);
        assert_eq!(header.fake_queue_size(), (8, 4096));
    }

    #[test]
    fn notify_names_the_queue() {
        let header = fake_header(2, 16);
        header.notify(1);
        assert_eq!(header.fake_notified(), 1);
    }

    #[test]
    fn the_config_space_starts_where_the_specification_says() {
        let header = fake_header(2, 16);
        assert_eq!(
            header.config_space() as usize - header as *const _ as usize,
            0x100
        );
    }

    #[test]
    fn the_header_does_not_reach_into_its_own_config_space() {
        // Every field added to the register map eats into the 0x100 bytes
        // before the config space; the day it does not fit, `config_space()`
        // starts returning a pointer into the header.
        assert!(core::mem::size_of::<VirtIOHeader>() <= 0x100);
    }
}

#[cfg(test)]
mod register_map_layout_tests {
    use super::*;
    use core::mem::{align_of, offset_of, size_of};

    // Every offset below is written out by hand from virtio 1.1, 4.2.2 "MMIO
    // Device Register Layout", and from 4.2.4 for the three legacy registers
    // this header still carries. None of them is computed from the struct: the
    // point is for the numbers to disagree with the declaration on the day the
    // declaration stops matching the specification.
    //
    // Nothing else in this crate can see these offsets. `fake_init` sets the
    // device's registers through the field *names*, and the `fake_*` readers
    // read them back the same way, so two fields of the same type trading
    // places cancels out everywhere -- `verify()` included, which would then
    // be comparing the magic value against the version register and the
    // version against the magic. All eleven such swaps survive the suite.

    #[test]
    fn the_four_identity_registers_come_first_in_the_specifications_order() {
        assert_eq!(offset_of!(VirtIOHeader, magic), 0x000);
        assert_eq!(offset_of!(VirtIOHeader, version), 0x004);
        assert_eq!(offset_of!(VirtIOHeader, device_id), 0x008);
        assert_eq!(offset_of!(VirtIOHeader, vendor_id), 0x00c);
    }

    #[test]
    fn each_side_of_the_feature_word_has_its_own_register_and_selector() {
        // The feature word is 64 bits read through a 32-bit register, so each
        // direction is a pair: the register and the half-selector that says
        // which half it holds. Swapping a register with its selector writes
        // the features into the selector and the selector into the features.
        assert_eq!(offset_of!(VirtIOHeader, device_features), 0x010);
        assert_eq!(offset_of!(VirtIOHeader, device_features_sel), 0x014);
        assert_eq!(offset_of!(VirtIOHeader, __r1), 0x018);
        assert_eq!(offset_of!(VirtIOHeader, driver_features), 0x020);
        assert_eq!(offset_of!(VirtIOHeader, driver_features_sel), 0x024);
    }

    #[test]
    fn the_three_legacy_registers_sit_where_the_legacy_interface_says() {
        // A modern device ignores these three and a legacy one needs all of
        // them, which is why they are holes in 4.2.2 and registers in 4.2.4.
        assert_eq!(offset_of!(VirtIOHeader, guest_page_size), 0x028);
        assert_eq!(offset_of!(VirtIOHeader, __r2), 0x02c);
        assert_eq!(offset_of!(VirtIOHeader, queue_align), 0x03c);
        assert_eq!(offset_of!(VirtIOHeader, queue_pfn), 0x040);
    }

    #[test]
    fn the_queue_selector_is_followed_by_the_size_the_device_offers_and_the_one_the_driver_takes() {
        assert_eq!(offset_of!(VirtIOHeader, queue_sel), 0x030);
        assert_eq!(offset_of!(VirtIOHeader, queue_num_max), 0x034);
        assert_eq!(offset_of!(VirtIOHeader, queue_num), 0x038);
        // The device answers in the first and the driver publishes in the
        // second, one word apart: the asking and the answering registers of
        // the same question, which is exactly the pair worth pinning.
        assert_eq!(
            offset_of!(VirtIOHeader, queue_num) - offset_of!(VirtIOHeader, queue_num_max),
            4
        );
    }

    #[test]
    fn queue_ready_follows_the_page_frame_number_it_replaced() {
        assert_eq!(offset_of!(VirtIOHeader, queue_ready), 0x044);
        assert_eq!(offset_of!(VirtIOHeader, __r3), 0x048);
        assert_eq!(offset_of!(VirtIOHeader, queue_notify), 0x050);
        assert_eq!(offset_of!(VirtIOHeader, __r4), 0x054);
    }

    #[test]
    fn the_interrupt_pair_and_the_status_are_where_a_device_raises_them() {
        assert_eq!(offset_of!(VirtIOHeader, interrupt_status), 0x060);
        assert_eq!(offset_of!(VirtIOHeader, interrupt_ack), 0x064);
        assert_eq!(offset_of!(VirtIOHeader, __r5), 0x068);
        assert_eq!(offset_of!(VirtIOHeader, status), 0x070);
        assert_eq!(offset_of!(VirtIOHeader, __r6), 0x074);
    }

    #[test]
    fn each_of_the_three_ring_addresses_is_written_low_half_first() {
        // These six registers are the only thing that tells the device where
        // the queue lives, and they are write-only: nothing reads them back,
        // so no round trip through the fake device can notice a pair that
        // traded places. The device would simply fetch descriptors from
        // (low << 32 | high) and walk somebody else's memory.
        assert_eq!(offset_of!(VirtIOHeader, queue_desc_low), 0x080);
        assert_eq!(offset_of!(VirtIOHeader, queue_desc_high), 0x084);
        assert_eq!(offset_of!(VirtIOHeader, __r7), 0x088);
        assert_eq!(offset_of!(VirtIOHeader, queue_avail_low), 0x090);
        assert_eq!(offset_of!(VirtIOHeader, queue_avail_high), 0x094);
        assert_eq!(offset_of!(VirtIOHeader, __r8), 0x098);
        assert_eq!(offset_of!(VirtIOHeader, queue_used_low), 0x0a0);
        assert_eq!(offset_of!(VirtIOHeader, queue_used_high), 0x0a4);
    }

    #[test]
    fn the_generation_counter_is_the_last_word_before_the_config_space() {
        // Twenty-one reserved words stand between the last ring address and
        // the generation counter, and that count is what puts the counter at
        // 0x0fc instead of somewhere inside the gap.
        assert_eq!(offset_of!(VirtIOHeader, __r9), 0x0a8);
        assert_eq!(offset_of!(VirtIOHeader, config_generation), 0x0fc);
        // Exactly 0x100, not "at most": `config_space()` adds that offset to
        // the header's own address, so the register map has to fill the window
        // to the byte, and a map that came up short would leave registers the
        // device drives unread rather than overlapping the config space.
        assert_eq!(size_of::<VirtIOHeader>(), CONFIG_SPACE_OFFSET);
        assert_eq!(size_of::<VirtIOHeader>(), 0x100);
        assert_eq!(align_of::<VirtIOHeader>(), 4);
    }
}
