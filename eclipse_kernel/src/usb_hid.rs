//! USB HID (Human Interface Device) Driver
//!
//! Supports USB keyboards and mice via the XHCI host controller.
//! Keyboard events are injected as PS/2 Set 1 scancodes via interrupts::push_key().
//! Mouse events are injected via interrupts::push_mouse_packet().
//!
//! ## Architecture
//! 1. `init()` detects XHCI controllers via PCI and calls `init_xhci_controller()`.
//! 2. `init_xhci_controller()` maps MMIO, resets the controller, allocates DMA rings,
//!    starts the controller, and enumerates ports.
//! 3. For each connected device, `enumerate_ports()` performs Enable Slot,
//!    Address Device, and reads the device descriptor.
//! 4. HID keyboards/mice get their interrupt IN endpoint configured, and the
//!    USB IRQ handler processes incoming reports, converting them to input events.

use core::sync::atomic::{fence, Ordering};
use core::ptr::{read_volatile, write_volatile};
use crate::memory::{map_mmio_range, alloc_dma_buffer};

// ===========================================================================
// USB Controller Detection Types
// ===========================================================================

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UsbControllerType {
    XHCI,
    EHCI,
    OHCI,
    UHCI,
}

impl UsbControllerType {
    pub fn as_str(&self) -> &'static str {
        match self {
            UsbControllerType::XHCI => "XHCI (USB 3.x)",
            UsbControllerType::EHCI => "EHCI (USB 2.0)",
            UsbControllerType::OHCI => "OHCI (USB 1.1)",
            UsbControllerType::UHCI => "UHCI (USB 1.1)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UsbControllerState {
    Uninitialized,
    Ready,
    Error,
}

pub struct UsbController {
    pub controller_type: UsbControllerType,
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    pub vendor_id: u16,
    pub device_id: u16,
    pub bar0: u64,
    pub interrupt_line: u8,
}

// ===========================================================================
// HID Constants and Report Structures
// ===========================================================================

pub const USB_CLASS_HID: u8       = 0x03;
pub const HID_SUBCLASS_NONE: u8   = 0x00;
pub const HID_SUBCLASS_BOOT: u8   = 0x01;
pub const HID_PROTOCOL_KEYBOARD: u8 = 0x01;
pub const HID_PROTOCOL_MOUSE: u8    = 0x02;

pub const USB_DESC_DEVICE: u8        = 0x01;
pub const USB_DESC_CONFIGURATION: u8 = 0x02;
pub const USB_DESC_INTERFACE: u8     = 0x04;
pub const USB_DESC_ENDPOINT: u8      = 0x05;
pub const USB_DESC_HID: u8           = 0x21;
pub const USB_DESC_HID_REPORT: u8    = 0x22;

pub const HID_REQUEST_SET_PROTOCOL: u8 = 0x0B;
pub const HID_REQUEST_SET_IDLE: u8     = 0x0A;

/// USB HID boot-protocol keyboard report (8 bytes).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct HidKeyboardReport {
    pub modifiers: u8,   // Modifier keys bitmask
    pub reserved: u8,    // Reserved (always 0)
    pub keys: [u8; 6],   // Up to 6 simultaneous key presses (HID usage IDs)
}

/// USB HID boot-protocol mouse report (4 bytes).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct HidMouseReport {
    pub buttons: u8,  // Button bitmask (bit0=L, bit1=R, bit2=M)
    pub x: i8,        // X displacement
    pub y: i8,        // Y displacement
    pub wheel: i8,    // Scroll wheel
}

// ===========================================================================
// HID Usage ID → PS/2 Set 1 Scancode Table
//
// Index = USB HID Usage ID (0x00–0xFF).
// Value = PS/2 Set 1 make code (0 = unmapped / extended key).
// Modifier keys (0xE0–0xE7) are handled separately via the modifier byte.
// ===========================================================================

pub static HID_TO_PS2: [u8; 256] = [
    //  0x00   0x01   0x02   0x03
        0,     0,     0,     0,
    // 0x04 a   0x05 b   0x06 c   0x07 d
       0x1E,  0x30,  0x2E,  0x20,
    // 0x08 e   0x09 f   0x0A g   0x0B h
       0x12,  0x21,  0x22,  0x23,
    // 0x0C i   0x0D j   0x0E k   0x0F l
       0x17,  0x24,  0x25,  0x26,
    // 0x10 m   0x11 n   0x12 o   0x13 p
       0x32,  0x31,  0x18,  0x19,
    // 0x14 q   0x15 r   0x16 s   0x17 t
       0x10,  0x13,  0x1F,  0x14,
    // 0x18 u   0x19 v   0x1A w   0x1B x
       0x16,  0x2F,  0x11,  0x2D,
    // 0x1C y   0x1D z
       0x15,  0x2C,
    // 0x1E 1   0x1F 2   0x20 3   0x21 4   0x22 5   0x23 6
       0x02,  0x03,  0x04,  0x05,  0x06,  0x07,
    // 0x24 7   0x25 8   0x26 9   0x27 0
       0x08,  0x09,  0x0A,  0x0B,
    // 0x28 Return  0x29 Escape  0x2A Backspace  0x2B Tab
       0x1C,  0x01,  0x0E,  0x0F,
    // 0x2C Space   0x2D -     0x2E =     0x2F [
       0x39,  0x0C,  0x0D,  0x1A,
    // 0x30 ]   0x31 backslash   0x32 Non-US #   0x33 ;
       0x1B,  0x2B,  0,     0x27,
    // 0x34 '   0x35 `     0x36 ,   0x37 .
       0x28,  0x29,  0x33,  0x34,
    // 0x38 /   0x39 CapsLock
       0x35,  0x3A,
    // 0x3A F1  0x3B F2  0x3C F3  0x3D F4  0x3E F5  0x3F F6
       0x3B,  0x3C,  0x3D,  0x3E,  0x3F,  0x40,
    // 0x40 F7  0x41 F8  0x42 F9  0x43 F10  0x44 F11  0x45 F12
       0x41,  0x42,  0x43,  0x44,  0x57,  0x58,
    // 0x46 PrintScreen  0x47 ScrollLock  0x48 Pause
       0,     0x46,  0,
    // 0x49 Insert  0x4A Home  0x4B PageUp  0x4C Delete  0x4D End  0x4E PageDown
       0,     0,     0,     0,     0,     0,
    // 0x4F RightArrow  0x50 LeftArrow  0x51 DownArrow  0x52 UpArrow
       0,     0,     0,     0,
    // 0x53 NumLock  0x54 KP/  0x55 KP*  0x56 KP-  0x57 KP+  0x58 KPEnter
       0x45,  0,     0x37,  0x4A,  0x4E,  0,
    // 0x59 KP1   0x5A KP2   0x5B KP3   0x5C KP4
       0x4F,  0x50,  0x51,  0x4B,
    // 0x5D KP5   0x5E KP6   0x5F KP7   0x60 KP8
       0x4C,  0x4D,  0x47,  0x48,
    // 0x61 KP9   0x62 KP0   0x63 KP.
       0x49,  0x52,  0x53,
    // 0x64–0xDF: unmapped (124 bytes of zero)
       0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // 0x64-0x6F
       0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // 0x70-0x7B
       0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // 0x7C-0x87
       0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // 0x88-0x93
       0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // 0x94-0x9F
       0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // 0xA0-0xAB
       0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // 0xAC-0xB7
       0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // 0xB8-0xC3
       0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // 0xC4-0xCF
       0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // 0xD0-0xDB
       0, 0, 0, 0,                           // 0xDC-0xDF
    // Modifier keys (reported in byte 0, but also appear at 0xE0-0xE7)
    // 0xE0 LCtrl  0xE1 LShift  0xE2 LAlt   0xE3 LGUI
       0x1D,  0x2A,  0x38,  0,
    // 0xE4 RCtrl  0xE5 RShift  0xE6 RAlt   0xE7 RGUI
       0,     0x36,  0,     0,
    // 0xE8–0xFF: unmapped (24 bytes)
       0, 0, 0, 0, 0, 0, 0, 0, // 0xE8-0xEF
       0, 0, 0, 0, 0, 0, 0, 0, // 0xF0-0xF7
       0, 0, 0, 0, 0, 0, 0, 0, // 0xF8-0xFF
];

// PS/2 Set 1 break code = make code | 0x80
// Modifier bitmask → PS/2 scancode mapping (bit index, make code)
const MODIFIER_SCANCODES: [(u8, u8); 6] = [
    (0, 0x1D), // Left Ctrl
    (1, 0x2A), // Left Shift
    (2, 0x38), // Left Alt
    (4, 0x1D), // Right Ctrl (same scancode as Left Ctrl for simplicity)
    (5, 0x36), // Right Shift
    (6, 0x38), // Right Alt (same scancode as Left Alt for simplicity)
];

// ===========================================================================
// MMIO Region – volatile register access with memory fences
// ===========================================================================

pub struct MmioRegion {
    pub base_virt: u64,
    pub size: usize,
}

impl MmioRegion {
    pub fn new(phys_addr: u64, size: usize) -> Result<Self, &'static str> {
        if phys_addr == 0 {
            return Err("Physical address is zero");
        }
        let virt_addr = map_mmio_range(phys_addr, size);
        Ok(Self { base_virt: virt_addr, size })
    }

    #[inline]
    pub fn read_u32(&self, offset: usize) -> u32 {
        let addr = (self.base_virt + offset as u64) as *const u32;
        fence(Ordering::Acquire);
        let v = unsafe { read_volatile(addr) };
        fence(Ordering::Acquire);
        v
    }

    #[inline]
    pub fn write_u32(&self, offset: usize, value: u32) {
        let addr = (self.base_virt + offset as u64) as *mut u32;
        fence(Ordering::Release);
        unsafe { write_volatile(addr, value); }
        fence(Ordering::Release);
    }
}

/// XHCI register regions (capability, operational, runtime, doorbell).
pub struct XhciMmio {
    pub capability:  MmioRegion,
    pub operational: MmioRegion,
    pub runtime:     MmioRegion,
    pub doorbell:    MmioRegion,
}

impl XhciMmio {
    pub fn from_bar0(bar0: u64) -> Result<Self, &'static str> {
        let cap = MmioRegion::new(bar0, 256)?;
        let caplength = (cap.read_u32(0x00) & 0xFF) as u64;
        let rtsoff    = cap.read_u32(0x18) as u64;
        let dboff     = cap.read_u32(0x14) as u64;

        let operational = MmioRegion::new(bar0 + caplength, 0x1000)?;
        let runtime     = MmioRegion::new(bar0 + rtsoff,    8192)?;
        let doorbell    = MmioRegion::new(bar0 + dboff,     1024)?;

        Ok(Self { capability: cap, operational, runtime, doorbell })
    }

    #[inline] pub fn read_capability (&self, off: usize) -> u32 { self.capability .read_u32(off) }
    #[inline] pub fn read_operational (&self, off: usize) -> u32 { self.operational.read_u32(off) }
    #[inline] pub fn write_operational(&self, off: usize, v: u32) { self.operational.write_u32(off, v) }
    #[inline] pub fn read_runtime     (&self, off: usize) -> u32 { self.runtime    .read_u32(off) }
    #[inline] pub fn write_runtime    (&self, off: usize, v: u32) { self.runtime    .write_u32(off, v) }

    pub fn ring_doorbell(&self, slot_id: u8, target: u8) {
        self.doorbell.write_u32((slot_id as usize) * 4, target as u32);
    }
}

// ===========================================================================
// DMA Allocation – physically contiguous buffers, intentionally leaked
// ===========================================================================

pub struct DmaAllocation {
    pub virt_addr: u64,
    pub phys_addr: u64,
    pub size: usize,
    pub alignment: usize,
}

impl DmaAllocation {
    pub fn allocate(size: usize, alignment: usize) -> Result<Self, &'static str> {
        let (ptr, phys) = alloc_dma_buffer(size, alignment)
            .ok_or("DMA buffer allocation failed")?;
        // Intentionally leaked – DMA buffers live for the device lifetime
        Ok(Self { virt_addr: ptr as u64, phys_addr: phys, size, alignment })
    }

    pub fn allocate_trb_ring(num_trbs: usize) -> Result<Self, &'static str> {
        Self::allocate(num_trbs * 16, 64)
    }

    pub fn allocate_dcbaa(max_slots: usize) -> Result<Self, &'static str> {
        Self::allocate((max_slots + 1) * 8, 4096)
    }

    pub fn zero(&self) {
        unsafe { core::ptr::write_bytes(self.virt_addr as *mut u8, 0, self.size); }
    }

    pub fn write_bytes(&self, offset: usize, data: &[u8]) {
        unsafe {
            core::ptr::copy_nonoverlapping(
                data.as_ptr(),
                (self.virt_addr + offset as u64) as *mut u8,
                data.len(),
            );
        }
    }

    pub fn read_bytes(&self, offset: usize, data: &mut [u8]) {
        unsafe {
            core::ptr::copy_nonoverlapping(
                (self.virt_addr + offset as u64) as *const u8,
                data.as_mut_ptr(),
                data.len(),
            );
        }
    }

    pub fn write_u32(&self, offset: usize, value: u32) {
        self.write_bytes(offset, &value.to_le_bytes());
    }

    pub fn write_u64(&self, offset: usize, value: u64) {
        self.write_bytes(offset, &value.to_le_bytes());
    }
}

// ===========================================================================
// TRB Structures and Ring Management
// ===========================================================================

pub const TRB_TYPE_NORMAL:             u8 = 1;
pub const TRB_TYPE_SETUP:              u8 = 2;
pub const TRB_TYPE_DATA:               u8 = 3;
pub const TRB_TYPE_STATUS:             u8 = 4;
pub const TRB_TYPE_LINK:               u8 = 6;
pub const TRB_TYPE_ENABLE_SLOT:        u8 = 9;
pub const TRB_TYPE_ADDRESS_DEVICE:     u8 = 11;
pub const TRB_TYPE_CONFIGURE_ENDPOINT: u8 = 12;
pub const TRB_TYPE_NOOP_COMMAND:       u8 = 23;

pub const TRB_COMPLETION_SUCCESS:      u8 = 1;
pub const TRB_COMPLETION_SHORT_PACKET: u8 = 13;

/// 16-byte Transfer Request Block.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Trb {
    pub parameter: u64,
    pub status:    u32,
    pub control:   u32,
}

impl Trb {
    pub const fn new() -> Self { Self { parameter: 0, status: 0, control: 0 } }

    pub fn get_trb_type(&self) -> u8 { ((self.control >> 10) & 0x3F) as u8 }
    pub fn cycle_bit(&self)    -> bool { (self.control & 1) != 0 }

    pub fn set_cycle_bit(&mut self, cycle: bool) {
        if cycle { self.control |= 1; } else { self.control &= !1; }
    }
}

/// Circular ring of TRBs backed by a DMA allocation.
pub struct TrbRing {
    pub allocation:    DmaAllocation,
    pub enqueue_index: usize,
    pub dequeue_index: usize,
    pub cycle_state:   bool,
    pub capacity:      usize,
    pub has_link_trb:  bool,
}

impl TrbRing {
    pub fn new(capacity: usize, has_link_trb: bool) -> Result<Self, &'static str> {
        let alloc = DmaAllocation::allocate_trb_ring(capacity)?;
        alloc.zero();

        if has_link_trb && capacity > 1 {
            let link = build_link_trb(alloc.phys_addr, true);
            let off = (capacity - 1) * 16;
            alloc.write_u64(off, link.parameter);
            alloc.write_u64(off + 8, (link.status as u64) | ((link.control as u64) << 32));
        }

        Ok(Self {
            allocation: alloc,
            enqueue_index: 0,
            dequeue_index: 0,
            cycle_state: true,
            capacity,
            has_link_trb,
        })
    }

    fn usable(&self) -> usize {
        if self.has_link_trb { self.capacity - 1 } else { self.capacity }
    }

    pub fn is_full(&self) -> bool {
        (self.enqueue_index + 1) % self.usable() == self.dequeue_index
    }

    /// Peek at the event TRB at the dequeue index (for Event Ring).
    pub fn peek_event(&self) -> Option<Trb> {
        let off = self.dequeue_index * 16;
        let mut raw = [0u8; 16];
        self.allocation.read_bytes(off, &mut raw);
        let control = u32::from_le_bytes([raw[12], raw[13], raw[14], raw[15]]);
        if (control & 1) as u8 == self.cycle_state as u8 {
            Some(Trb {
                parameter: u64::from_le_bytes(raw[0..8].try_into().unwrap()),
                status:    u32::from_le_bytes(raw[8..12].try_into().unwrap()),
                control,
            })
        } else {
            None
        }
    }

    /// Advance the dequeue index after consuming an event TRB.
    pub fn pop_event(&mut self) -> Option<Trb> {
        let trb = self.peek_event()?;
        self.dequeue_index = (self.dequeue_index + 1) % self.capacity;
        if self.dequeue_index == 0 { self.cycle_state = !self.cycle_state; }
        Some(trb)
    }

    /// Enqueue a TRB with the current cycle bit (for Command/Transfer Ring).
    pub fn enqueue(&mut self, mut trb: Trb) -> Result<(), &'static str> {
        if self.is_full() { return Err("TRB ring is full"); }
        trb.set_cycle_bit(self.cycle_state);
        let off = self.enqueue_index * 16;
        self.allocation.write_u64(off, trb.parameter);
        self.allocation.write_u64(off + 8, (trb.status as u64) | ((trb.control as u64) << 32));
        self.enqueue_index = (self.enqueue_index + 1) % self.usable();
        if self.enqueue_index == 0 { self.cycle_state = !self.cycle_state; }
        Ok(())
    }
}

/// Command Ring (host → controller commands).
pub struct CommandRing {
    pub ring: TrbRing,
}

impl CommandRing {
    pub fn new(capacity: usize) -> Result<Self, &'static str> {
        Ok(Self { ring: TrbRing::new(capacity, true)? })
    }

    pub fn submit(&mut self, trb: Trb) -> Result<u64, &'static str> {
        let phys = self.ring.allocation.phys_addr + (self.ring.enqueue_index as u64 * 16);
        self.ring.enqueue(trb)?;
        Ok(phys)
    }

    pub fn get_crcr(&self) -> u64 { self.ring.allocation.phys_addr }
}

/// Transfer Ring (per-endpoint transfers).
pub struct TransferRing {
    pub ring:             TrbRing,
    pub endpoint_address: u8,
    pub device_slot:      u8,
}

impl TransferRing {
    pub fn new(capacity: usize, device_slot: u8, endpoint_address: u8) -> Result<Self, &'static str> {
        Ok(Self { ring: TrbRing::new(capacity, true)?, endpoint_address, device_slot })
    }

    pub fn submit(&mut self, trb: Trb) -> Result<u64, &'static str> {
        let phys = self.ring.allocation.phys_addr + (self.ring.enqueue_index as u64 * 16);
        self.ring.enqueue(trb)?;
        Ok(phys)
    }

    pub fn get_address(&self) -> u64 { self.ring.allocation.phys_addr }
}

/// Event Ring (controller → host events) with Event Ring Segment Table.
pub struct EventRing {
    pub ring: TrbRing,
    pub erst: DmaAllocation,
}

impl EventRing {
    pub fn new(capacity: usize) -> Result<Self, &'static str> {
        let ring = TrbRing::new(capacity, false)?;
        let erst = DmaAllocation::allocate(16, 64)?; // 1-entry ERST
        erst.zero();
        // ERST entry: [base_addr 8B][segment_size 4B][reserved 4B]
        erst.write_u64(0,  ring.allocation.phys_addr);
        erst.write_u32(8,  capacity as u32);
        erst.write_u32(12, 0);
        Ok(Self { ring, erst })
    }

    pub fn get_erdp(&self) -> u64 {
        self.ring.allocation.phys_addr + (self.ring.dequeue_index as u64 * 16)
    }

    pub fn get_erst_base(&self) -> u64 { self.erst.phys_addr }

    pub fn process_next_event(&mut self) -> Option<Trb> {
        self.ring.pop_event()
    }
}

// ===========================================================================
// XHCI Context Structures (Section 6.2 of XHCI 1.2 spec)
// ===========================================================================

#[repr(C, align(32))]
#[derive(Clone, Copy)]
pub struct SlotContext {
    pub route_string_and_speed: u32,
    pub port_info: u32,
    pub port_and_hub: u32,
    pub slot_state: u32,
    _reserved: [u32; 4],
}

impl SlotContext {
    pub const fn new() -> Self {
        Self { route_string_and_speed: 0, port_info: 0, port_and_hub: 0, slot_state: 0, _reserved: [0; 4] }
    }
}

#[repr(C, align(32))]
#[derive(Clone, Copy)]
pub struct EndpointContext {
    pub ep_state: u32,
    pub ep_info: u32,
    pub tr_dequeue_pointer: u64,
    pub avg_trb_length_and_max_esit: u32,
    _reserved: [u32; 3],
}

impl EndpointContext {
    pub const fn new() -> Self {
        Self { ep_state: 0, ep_info: 0, tr_dequeue_pointer: 0, avg_trb_length_and_max_esit: 0, _reserved: [0; 3] }
    }
}

pub const EP_TYPE_CONTROL: u8   = 4;
pub const EP_TYPE_INTERRUPT_IN: u8 = 7;

#[repr(C, align(64))]
pub struct DeviceContext {
    pub slot_context: SlotContext,
    pub endpoint_contexts: [EndpointContext; 31],
}

impl DeviceContext {
    pub fn new() -> Self {
        Self { slot_context: SlotContext::new(), endpoint_contexts: [EndpointContext::new(); 31] }
    }
}

#[repr(C)]
pub struct InputControlContext {
    pub drop_flags: u32,
    pub add_flags: u32,
    _reserved: [u32; 5],
    pub config_value: u8,
    pub interface_number: u8,
    pub alternate_setting: u8,
    _reserved2: u8,
}

impl InputControlContext {
    pub fn new() -> Self {
        Self { drop_flags: 0, add_flags: 0, _reserved: [0; 5], config_value: 0, interface_number: 0, alternate_setting: 0, _reserved2: 0 }
    }
}

#[repr(C, align(64))]
pub struct InputContext {
    pub control: InputControlContext,
    pub device: DeviceContext,
}

impl InputContext {
    pub fn new() -> Self {
        Self { control: InputControlContext::new(), device: DeviceContext::new() }
    }
}

// ===========================================================================
// TRB Builder Functions
// ===========================================================================

pub fn build_link_trb(next_segment: u64, toggle_cycle: bool) -> Trb {
    let mut ctrl = (TRB_TYPE_LINK as u32) << 10;
    if toggle_cycle { ctrl |= 0x2; }
    Trb { parameter: next_segment, status: 0, control: ctrl }
}

pub fn build_enable_slot_trb(slot_type: u8) -> Trb {
    Trb {
        parameter: 0,
        status: 0,
        control: ((TRB_TYPE_ENABLE_SLOT as u32) << 10) | ((slot_type as u32) << 16),
    }
}

pub fn build_address_device_trb(input_ctx_ptr: u64, slot_id: u8, bsr: bool) -> Trb {
    let mut ctrl = ((TRB_TYPE_ADDRESS_DEVICE as u32) << 10) | ((slot_id as u32) << 24);
    if bsr { ctrl |= 0x200; }
    Trb { parameter: input_ctx_ptr, status: 0, control: ctrl }
}

pub fn build_configure_endpoint_trb(input_ctx_ptr: u64, slot_id: u8) -> Trb {
    Trb {
        parameter: input_ctx_ptr,
        status: 0,
        control: ((TRB_TYPE_CONFIGURE_ENDPOINT as u32) << 10) | ((slot_id as u32) << 24),
    }
}

pub fn build_setup_trb(bmrt: u8, breq: u8, wvalue: u16, windex: u16, wlength: u16, trt: u8) -> Trb {
    let param = (bmrt as u64)
        | ((breq as u64) << 8)
        | ((wvalue as u64) << 16)
        | ((windex as u64) << 32)
        | ((wlength as u64) << 48);
    Trb { parameter: param, status: 8, control: (2u32 << 10) | (1u32 << 6) | ((trt as u32) << 16) }
}

pub fn build_data_trb(buf_ptr: u64, length: u32, is_in: bool) -> Trb {
    let mut ctrl = 3u32 << 10;
    if is_in { ctrl |= 1 << 16; }
    Trb { parameter: buf_ptr, status: length & 0x1FFFF, control: ctrl }
}

pub fn build_status_trb(is_in: bool, ioc: bool) -> Trb {
    let mut ctrl = 4u32 << 10;
    if is_in { ctrl |= 1 << 16; }
    if ioc { ctrl |= 1 << 5; }
    Trb { parameter: 0, status: 0, control: ctrl }
}

pub fn build_normal_trb(buf_ptr: u64, length: u16, ioc: bool) -> Trb {
    let mut ctrl = (TRB_TYPE_NORMAL as u32) << 10;
    if ioc { ctrl |= 1 << 5; }
    Trb { parameter: buf_ptr, status: (length as u32) & 0x1FFFF, control: ctrl }
}

// ===========================================================================
// HID Device Tracking
// ===========================================================================

/// Info about an active HID device's interrupt IN endpoint.
pub struct HidEndpoint {
    pub slot_id:      u8,
    pub endpoint_id:  u8,   // XHCI endpoint ID (1-indexed, 2*ep_num+dir)
    pub protocol:     u8,   // HID_PROTOCOL_KEYBOARD or HID_PROTOCOL_MOUSE
    pub buf_virt:     u64,  // Virtual address of DMA report buffer
    pub buf_phys:     u64,  // Physical address
    pub buf_size:     usize,
}

const MAX_HID_DEVICES: usize = 8;
static mut HID_DEVICES: [Option<HidEndpoint>; MAX_HID_DEVICES] = [
    None, None, None, None, None, None, None, None,
];

static mut LAST_KEYBOARD_REPORT: HidKeyboardReport = HidKeyboardReport { modifiers: 0, reserved: 0, keys: [0; 6] };
static mut LAST_MOUSE_REPORT: HidMouseReport = HidMouseReport { buttons: 0, x: 0, y: 0, wheel: 0 };

// ===========================================================================
// XHCI Controller State
// ===========================================================================

pub struct XhciControllerState {
    pub mmio:            Option<XhciMmio>,
    pub command_ring:    Option<CommandRing>,
    pub event_rings:     alloc::vec::Vec<EventRing>,
    pub device_contexts: alloc::vec::Vec<Option<DmaAllocation>>,
    pub transfer_rings:  alloc::vec::Vec<Option<TransferRing>>,
    pub dcbaa:           Option<DmaAllocation>,
    pub dcbaa_phys:      u64,
    pub mmio_base:       u64,
    pub max_slots:       u8,
    pub max_ports:       u8,
}

impl XhciControllerState {
    pub fn new(mmio_base: u64, mmio: XhciMmio) -> Self {
        Self {
            mmio: Some(mmio),
            command_ring: None,
            event_rings: alloc::vec::Vec::new(),
            device_contexts: alloc::vec::Vec::new(),
            transfer_rings: alloc::vec::Vec::new(),
            dcbaa: None,
            dcbaa_phys: 0,
            mmio_base,
            max_slots: 0,
            max_ports: 0,
        }
    }

    /// Allocate DCBAA, command ring, event ring, then reset and start the controller.
    pub fn initialize(&mut self) -> Result<(), &'static str> {
        crate::serial::serial_print("[XHCI] Initializing rings and structures\n");
        let mmio = self.mmio.as_ref().ok_or("MMIO not found")?;

        // 1. DCBAA
        let max_slots = self.max_slots as usize;
        let dcbaa = DmaAllocation::allocate_dcbaa(max_slots)?;
        dcbaa.zero();

        // 2. Scratchpad buffers if required
        let hcsparams2 = mmio.read_capability(0x08);
        let sb_count = ((hcsparams2 >> 27) & 0x1F) | ((hcsparams2 >> 16) & 0x1F);
        if sb_count > 0 {
            crate::serial::serial_print(&alloc::format!("[XHCI] {} scratchpad buffers\n", sb_count));
            let sb_array = DmaAllocation::allocate((sb_count as usize) * 8, 64)?;
            sb_array.zero();
            for i in 0..sb_count as usize {
                let page = DmaAllocation::allocate(4096, 4096)?;
                page.zero();
                sb_array.write_u64(i * 8, page.phys_addr);
            }
            dcbaa.write_u64(0, sb_array.phys_addr);
        }

        self.dcbaa_phys = dcbaa.phys_addr;
        self.dcbaa = Some(dcbaa);

        // 3. Command ring (64 TRBs)
        self.command_ring = Some(CommandRing::new(64)?);

        // 4. Event ring (256 TRBs)
        self.event_rings.push(EventRing::new(256)?);

        // 5. Pre-size device context + transfer ring arrays
        for _ in 0..=max_slots {
            self.device_contexts.push(None);
        }
        for _ in 0..=(max_slots * 32) {
            self.transfer_rings.push(None);
        }

        crate::serial::serial_print("[XHCI] Rings initialized\n");
        Ok(())
    }

    /// Reset the controller and program all datastructure base addresses, then Run.
    pub fn start_controller(&mut self) -> Result<(), &'static str> {
        crate::serial::serial_print("[XHCI] Starting controller\n");
        let mmio = self.mmio.as_ref().ok_or("MMIO not initialized")?;

        // Wait for CNR
        let mut timeout = 10000;
        while (mmio.read_operational(0x04) & (1 << 11)) != 0 {
            timeout -= 1;
            if timeout == 0 { return Err("CNR timeout before reset"); }
        }

        // HCRST
        mmio.write_operational(0x00, mmio.read_operational(0x00) | (1 << 1));
        timeout = 10000;
        while (mmio.read_operational(0x00) & (1 << 1)) != 0 {
            timeout -= 1;
            if timeout == 0 { return Err("HCRST timeout"); }
        }

        // CNR clear again
        timeout = 10000;
        while (mmio.read_operational(0x04) & (1 << 11)) != 0 {
            timeout -= 1;
            if timeout == 0 { return Err("CNR timeout after reset"); }
        }
        crate::serial::serial_print("[XHCI] Reset complete\n");

        // MaxSlotsEnabled
        let cfg = mmio.read_operational(0x38);
        mmio.write_operational(0x38, (cfg & !0xFF) | (self.max_slots as u32));

        // DCBAAP
        mmio.write_operational(0x30, self.dcbaa_phys as u32);
        mmio.write_operational(0x34, (self.dcbaa_phys >> 32) as u32);

        // CRCR
        if let Some(ref cr) = self.command_ring {
            let crcr = cr.get_crcr();
            mmio.write_operational(0x18, (crcr as u32) | 1);
            mmio.write_operational(0x1C, (crcr >> 32) as u32);
        }

        // Event ring
        if !self.event_rings.is_empty() {
            let er = &self.event_rings[0];
            let erdp = er.get_erdp();
            let erstba = er.get_erst_base();
            mmio.write_runtime(0x28, 1);
            mmio.write_runtime(0x38, erdp as u32);
            mmio.write_runtime(0x3C, (erdp >> 32) as u32);
            mmio.write_runtime(0x30, erstba as u32);
            mmio.write_runtime(0x34, (erstba >> 32) as u32);
        }

        // Enable interrupter 0 and interrupts, then Run
        mmio.write_runtime(0x20, 0x03); // IMAN IE+IP
        mmio.write_runtime(0x24, 0);    // IMOD = 0

        let cmd = mmio.read_operational(0x00) | (1 << 2) | 1; // INTE + R/S
        mmio.write_operational(0x00, cmd);

        crate::serial::serial_print("[XHCI] Controller running\n");
        Ok(())
    }

    /// Submit a command TRB and ring doorbell 0.
    pub fn submit_command(&mut self, trb: Trb) -> Result<u64, &'static str> {
        let phys = self.command_ring.as_mut().ok_or("No command ring")?.submit(trb)?;
        if let Some(ref mmio) = self.mmio { mmio.ring_doorbell(0, 0); }
        Ok(phys)
    }

    /// Spin-poll the event ring for a matching TRB (by type filter).
    pub fn poll_event(&mut self, target_phys: u64, type_filter: u8) -> Result<Trb, &'static str> {
        let mut timeout = 5_000_000usize;
        while timeout > 0 {
            if let Some(er) = self.event_rings.get_mut(0) {
                if let Some(ev) = er.ring.peek_event() {
                    let trb_type = ev.get_trb_type();
                    let erdp = er.get_erdp();
                    er.ring.pop_event();

                    // Update ERDP (clear EHB)
                    if let Some(ref mmio) = self.mmio {
                        mmio.write_runtime(0x38, (erdp as u32) | 0x08);
                        mmio.write_runtime(0x3C, (erdp >> 32) as u32);
                    }

                    if trb_type == type_filter {
                        let matches = target_phys == 0 || ev.parameter == target_phys;
                        if matches {
                            let code = (ev.status >> 24) & 0xFF;
                            if code == TRB_COMPLETION_SUCCESS as u32
                                || code == TRB_COMPLETION_SHORT_PACKET as u32
                            {
                                return Ok(ev);
                            } else {
                                return Err("Command/transfer failed");
                            }
                        }
                    }
                }
            }
            timeout -= 1;
        }
        Err("Poll timeout")
    }

    /// Submit a command TRB and wait for its Command Completion Event.
    pub fn execute_command(&mut self, trb: Trb) -> Result<Trb, &'static str> {
        let phys = self.submit_command(trb)?;
        self.poll_event(phys, 33) // 33 = Command Completion Event
    }

    /// Enumerate all ports, reset connected ones, and address devices.
    pub fn enumerate_ports(&mut self) -> Result<(), &'static str> {
        crate::serial::serial_print("[XHCI] Enumerating ports\n");
        let max_ports = self.max_ports;

        for port in 1..=max_ports {
            let port_off = 0x400 + (port as usize - 1) * 0x10;

            let (is_connected, portsc) = {
                let mmio = self.mmio.as_ref().ok_or("No MMIO")?;
                let v = mmio.read_operational(port_off);
                ((v & 1) != 0, v)
            };

            if !is_connected { continue; }

            crate::serial::serial_print(&alloc::format!("[XHCI] Port {} connected\n", port));

            // Assert Port Reset
            if let Some(ref mmio) = self.mmio {
                mmio.write_operational(port_off, portsc | (1 << 4));
            }

            // Wait for PRC (bit 21) or PR clear (bit 4)
            let mut done = false;
            for _ in 0..100_000 {
                let mmio = self.mmio.as_ref().ok_or("No MMIO")?;
                let s = mmio.read_operational(port_off);
                if (s & (1 << 21)) != 0 || (s & (1 << 4)) == 0 { done = true; break; }
            }
            if !done {
                crate::serial::serial_print(&alloc::format!("[XHCI] Port {} reset timeout\n", port));
                continue;
            }

            let (port_speed, new_portsc) = {
                let mmio = self.mmio.as_ref().ok_or("No MMIO")?;
                let s = mmio.read_operational(port_off);
                let spd = (s >> 10) & 0x0F;
                let clear = (1 << 17) | (1 << 18) | (1 << 19) | (1 << 20) | (1 << 21) | (1 << 22);
                mmio.write_operational(port_off, (s & 0x0E00C3E0) | clear);
                (spd, s)
            };
            let _ = new_portsc;
            crate::serial::serial_print(&alloc::format!("[XHCI] Port {} speed={}\n", port, port_speed));

            // Enable Slot
            let ev = match self.execute_command(build_enable_slot_trb(0)) {
                Ok(e) => e,
                Err(e) => { crate::serial::serial_print(&alloc::format!("[XHCI] Enable Slot failed: {}\n", e)); continue; }
            };
            let slot_id = ((ev.control >> 24) & 0xFF) as u8;
            crate::serial::serial_print(&alloc::format!("[XHCI] Slot ID: {}\n", slot_id));

            // Allocate device context
            let dev_ctx = match DmaAllocation::allocate(1024, 64) {
                Ok(d) => d,
                Err(e) => { crate::serial::serial_print(&alloc::format!("[XHCI] Dev ctx alloc failed: {}\n", e)); continue; }
            };
            dev_ctx.zero();
            if let Some(ref dcbaa) = self.dcbaa {
                dcbaa.write_u64(slot_id as usize * 8, dev_ctx.phys_addr);
            }
            let slot_idx = slot_id as usize;
            self.device_contexts[slot_idx] = Some(dev_ctx);

            // Allocate input context
            let input_ctx = match DmaAllocation::allocate(2048, 64) {
                Ok(ic) => ic,
                Err(e) => { crate::serial::serial_print(&alloc::format!("[XHCI] Input ctx alloc failed: {}\n", e)); continue; }
            };
            input_ctx.zero();
            // Add flags: Slot (bit 0) + EP0 (bit 1)
            input_ctx.write_u32(4, 0x03);
            // Slot context: speed + 1 context entry
            let route = (port_speed << 20) | (1 << 27);
            input_ctx.write_u32(32, route);
            input_ctx.write_u32(36, (port as u32) << 16);
            // EP0 context
            let mps: u32 = match port_speed { 2 => 8, 4 => 512, _ => 64 };
            input_ctx.write_u32(68, (3 << 1) | (EP_TYPE_CONTROL as u32) << 3 | (mps << 16));

            // EP0 transfer ring
            let ep0_ring = match TransferRing::new(64, slot_id, 0) {
                Ok(r) => r,
                Err(e) => { crate::serial::serial_print(&alloc::format!("[XHCI] TR alloc failed: {}\n", e)); continue; }
            };
            input_ctx.write_u64(72, ep0_ring.get_address() | 1);
            self.transfer_rings[slot_idx * 32] = Some(ep0_ring);

            // Address Device
            let addr_ev = match self.execute_command(build_address_device_trb(input_ctx.phys_addr, slot_id, false)) {
                Ok(e) => e,
                Err(e) => { crate::serial::serial_print(&alloc::format!("[XHCI] Address Device failed: {}\n", e)); continue; }
            };
            let _ = addr_ev;
            crate::serial::serial_print(&alloc::format!("[XHCI] Device addressed (slot {})\n", slot_id));

            // Get device descriptor
            if let Err(e) = self.get_device_descriptor(slot_id) {
                crate::serial::serial_print(&alloc::format!("[XHCI] GetDescriptor failed: {}\n", e));
                continue;
            }

            // Try to set up HID endpoint
            if let Err(e) = self.setup_hid(slot_id) {
                crate::serial::serial_print(&alloc::format!("[XHCI] HID setup failed (may not be HID): {}\n", e));
            }
        }
        Ok(())
    }

    /// Perform a control transfer to read the device descriptor (first 18 bytes).
    fn get_device_descriptor(&mut self, slot_id: u8) -> Result<[u8; 18], &'static str> {
        let desc_buf = DmaAllocation::allocate(64, 64)?;
        desc_buf.zero();

        let setup = build_setup_trb(0x80, 0x06, 0x0100, 0x0000, 18, 3);
        let data  = build_data_trb(desc_buf.phys_addr, 18, true);
        let stat  = build_status_trb(false, true);

        let status_phys = {
            let slot_idx = slot_id as usize;
            let tr = self.transfer_rings[slot_idx * 32].as_mut().ok_or("No EP0 ring")?;
            tr.submit(setup)?;
            tr.submit(data)?;
            tr.submit(stat)?
        };

        if let Some(ref mmio) = self.mmio { mmio.ring_doorbell(slot_id, 1); }

        self.poll_event(status_phys, 32)?; // 32 = Transfer Event

        let mut raw = [0u8; 18];
        desc_buf.read_bytes(0, &mut raw);
        crate::serial::serial_print(&alloc::format!(
            "[XHCI] Device: bcdUSB={:02X}{:02X} class={:02X} VID={:02X}{:02X} PID={:02X}{:02X}\n",
            raw[3], raw[2], raw[4], raw[9], raw[8], raw[11], raw[10]
        ));
        Ok(raw)
    }

    /// Read the configuration descriptor and set up HID interrupt endpoints.
    fn setup_hid(&mut self, slot_id: u8) -> Result<(), &'static str> {
        // Read first 9 bytes to get wTotalLength
        let cfg_buf = DmaAllocation::allocate(256, 64)?;
        cfg_buf.zero();

        let setup = build_setup_trb(0x80, 0x06, 0x0200, 0x0000, 9, 3);
        let data  = build_data_trb(cfg_buf.phys_addr, 9, true);
        let stat  = build_status_trb(false, true);

        let stat_phys = {
            let tr = self.transfer_rings[slot_id as usize * 32].as_mut().ok_or("No EP0 ring")?;
            tr.submit(setup)?;
            tr.submit(data)?;
            tr.submit(stat)?
        };
        if let Some(ref mmio) = self.mmio { mmio.ring_doorbell(slot_id, 1); }
        self.poll_event(stat_phys, 32)?;

        let mut header = [0u8; 9];
        cfg_buf.read_bytes(0, &mut header);
        let total_len = u16::from_le_bytes([header[2], header[3]]) as usize;
        if total_len < 9 || total_len > 256 { return Err("Invalid configuration descriptor length"); }

        // Read full configuration descriptor
        cfg_buf.zero();
        let setup2 = build_setup_trb(0x80, 0x06, 0x0200, 0x0000, total_len as u16, 3);
        let data2  = build_data_trb(cfg_buf.phys_addr, total_len as u32, true);
        let stat2  = build_status_trb(false, true);

        let stat2_phys = {
            let tr = self.transfer_rings[slot_id as usize * 32].as_mut().ok_or("No EP0 ring")?;
            tr.submit(setup2)?;
            tr.submit(data2)?;
            tr.submit(stat2)?
        };
        if let Some(ref mmio) = self.mmio { mmio.ring_doorbell(slot_id, 1); }
        self.poll_event(stat2_phys, 32)?;

        let mut cfg_data = [0u8; 256];
        cfg_buf.read_bytes(0, &mut cfg_data[..total_len]);

        // Parse descriptors to find HID interrupt IN endpoint
        let mut offset = 0usize;
        let mut hid_protocol = 0u8;
        let mut ep_addr = 0u8;
        let mut ep_interval = 10u8;
        let mut ep_mps = 8u16;

        while offset < total_len {
            if offset + 2 > total_len { break; }
            let desc_len  = cfg_data[offset] as usize;
            let desc_type = cfg_data[offset + 1];
            if desc_len < 2 { break; }

            if desc_type == USB_DESC_INTERFACE && offset + 9 <= total_len {
                let iface_class    = cfg_data[offset + 5];
                let iface_subclass = cfg_data[offset + 6];
                let iface_protocol = cfg_data[offset + 7];
                if iface_class == USB_CLASS_HID && iface_subclass == HID_SUBCLASS_BOOT {
                    hid_protocol = iface_protocol;
                }
            }

            if desc_type == USB_DESC_ENDPOINT && offset + 7 <= total_len && hid_protocol != 0 {
                let addr       = cfg_data[offset + 2];
                let attributes = cfg_data[offset + 3];
                let mps        = u16::from_le_bytes([cfg_data[offset + 4], cfg_data[offset + 5]]);
                let interval   = cfg_data[offset + 6];
                // We want an Interrupt IN endpoint
                if (addr & 0x80) != 0 && (attributes & 0x03) == 0x03 {
                    ep_addr     = addr;
                    ep_mps      = mps;
                    ep_interval = interval;
                }
            }

            offset += desc_len;
        }

        if ep_addr == 0 {
            return Err("No HID interrupt IN endpoint found");
        }

        crate::serial::serial_print(&alloc::format!(
            "[XHCI] HID slot={} protocol={} ep=0x{:02X} mps={} interval={}\n",
            slot_id, hid_protocol, ep_addr, ep_mps, ep_interval
        ));

        // SET_CONFIGURATION (value 1)
        let setcfg_setup = build_setup_trb(0x00, 0x09, 1, 0, 0, 0);
        let setcfg_stat  = build_status_trb(true, true);
        let setcfg_phys = {
            let tr = self.transfer_rings[slot_id as usize * 32].as_mut().ok_or("No EP0 ring")?;
            tr.submit(setcfg_setup)?;
            tr.submit(setcfg_stat)?
        };
        if let Some(ref mmio) = self.mmio { mmio.ring_doorbell(slot_id, 1); }
        let _ = self.poll_event(setcfg_phys, 32);

        // SET_PROTOCOL = Boot Protocol (0)
        let prot_setup = build_setup_trb(0x21, HID_REQUEST_SET_PROTOCOL, 0, 0, 0, 0);
        let prot_stat  = build_status_trb(true, true);
        let prot_phys = {
            let tr = self.transfer_rings[slot_id as usize * 32].as_mut().ok_or("No EP0 ring")?;
            tr.submit(prot_setup)?;
            tr.submit(prot_stat)?
        };
        if let Some(ref mmio) = self.mmio { mmio.ring_doorbell(slot_id, 1); }
        let _ = self.poll_event(prot_phys, 32);

        // SET_IDLE = 0 (report on change only)
        let idle_setup = build_setup_trb(0x21, HID_REQUEST_SET_IDLE, 0, 0, 0, 0);
        let idle_stat  = build_status_trb(true, true);
        let idle_phys = {
            let tr = self.transfer_rings[slot_id as usize * 32].as_mut().ok_or("No EP0 ring")?;
            tr.submit(idle_setup)?;
            tr.submit(idle_stat)?
        };
        if let Some(ref mmio) = self.mmio { mmio.ring_doorbell(slot_id, 1); }
        let _ = self.poll_event(idle_phys, 32);

        // Calculate XHCI endpoint ID
        let ep_number = ep_addr & 0x0F;
        let ep_dir_in = (ep_addr & 0x80) != 0;
        let xhci_ep_id = ep_number * 2 + if ep_dir_in { 1 } else { 0 };

        // Build Configure Endpoint input context
        let cfg_ctx = DmaAllocation::allocate(2048, 64)?;
        cfg_ctx.zero();
        // Add: Slot context (bit 0) + new endpoint context (bit xhci_ep_id)
        cfg_ctx.write_u32(4, 0x01 | (1u32 << xhci_ep_id));
        // Slot context: update Context Entries to highest enabled DCI
        cfg_ctx.write_u32(32, (port_speed_from_slot(slot_id) << 20) | ((xhci_ep_id as u32) << 27));
        // Endpoint context DWORD1: ErrorCount=3, EPType=Interrupt IN, MaxPacketSize
        let ep_dword2: u32 = ((ep_mps as u32) << 16) // MaxPacketSize (bits 31:16)
            | (3 << 1)                                 // Error Count (bits 2:1) = 3
            | ((EP_TYPE_INTERRUPT_IN as u32) << 3);   // EP Type (bits 5:3) = 7

        // Endpoint context offset in input context: 32 (icc) + 32 (slot) + (xhci_ep_id-1)*32
        let ep_ctx_off = 32 + 32 + (xhci_ep_id as usize - 1) * 32;

        // Allocate interrupt IN transfer ring
        let int_ring = TransferRing::new(64, slot_id, ep_addr)?;
        let int_ring_phys = int_ring.get_address();
        cfg_ctx.write_u32(ep_ctx_off + 4, ep_dword2);
        cfg_ctx.write_u64(ep_ctx_off + 8, int_ring_phys | 1); // DCS = 1

        let ring_idx = slot_id as usize * 32 + xhci_ep_id as usize;
        if ring_idx < self.transfer_rings.len() {
            self.transfer_rings[ring_idx] = Some(int_ring);
        }

        // Submit Configure Endpoint command
        if let Err(e) = self.execute_command(build_configure_endpoint_trb(cfg_ctx.phys_addr, slot_id)) {
            crate::serial::serial_print(&alloc::format!("[XHCI] Configure Endpoint failed: {}\n", e));
            return Err("Configure Endpoint failed");
        }

        // Allocate HID report buffer
        let report_buf_size = if hid_protocol == HID_PROTOCOL_KEYBOARD { 8 } else { 4 };
        let report_buf = DmaAllocation::allocate(report_buf_size, 64)?;
        report_buf.zero();

        // Submit initial Normal TRB for interrupt IN endpoint
        let normal = build_normal_trb(report_buf.phys_addr, report_buf_size as u16, true);
        let ring_idx2 = slot_id as usize * 32 + xhci_ep_id as usize;
        if let Some(Some(ref mut tr)) = self.transfer_rings.get_mut(ring_idx2) {
            let _ = tr.submit(normal);
        }
        if let Some(ref mmio) = self.mmio { mmio.ring_doorbell(slot_id, xhci_ep_id); }

        // Register HID device
        register_hid_device(HidEndpoint {
            slot_id,
            endpoint_id: xhci_ep_id,
            protocol: hid_protocol,
            buf_virt: report_buf.phys_addr, // intentionally using phys here for simplicity; virt = HHDM + phys
            buf_phys: report_buf.phys_addr,
            buf_size: report_buf_size,
        });

        crate::serial::serial_print(&alloc::format!("[XHCI] HID device registered (slot={} proto={})\n", slot_id, hid_protocol));
        Ok(())
    }

    /// Process pending events (called from interrupt handler).
    pub fn process_events(&mut self) {
        if self.event_rings.is_empty() { return; }

        loop {
            let ev = {
                let er = &mut self.event_rings[0];
                match er.ring.peek_event() {
                    Some(e) => {
                        let erdp = er.get_erdp();
                        er.ring.pop_event();
                        if let Some(ref mmio) = self.mmio {
                            mmio.write_runtime(0x38, (erdp as u32) | 0x08);
                            mmio.write_runtime(0x3C, (erdp >> 32) as u32);
                        }
                        e
                    }
                    None => break,
                }
            };

            let trb_type = ev.get_trb_type();
            if trb_type == 32 {
                // Transfer Event: find which HID device completed
                let slot_id   = ((ev.control >> 24) & 0xFF) as u8;
                let ep_id     = ((ev.control >> 16) & 0x1F) as u8;
                process_hid_transfer_event(slot_id, ep_id);

                // Re-submit the Normal TRB for continuous polling
                let ring_idx = slot_id as usize * 32 + ep_id as usize;
                if let Some(Some(ref mut tr)) = self.transfer_rings.get_mut(ring_idx) {
                    let buf_phys = find_hid_buf_phys(slot_id, ep_id);
                    let buf_size = find_hid_buf_size(slot_id, ep_id);
                    if buf_phys != 0 {
                        let normal = build_normal_trb(buf_phys, buf_size as u16, true);
                        let _ = tr.submit(normal);
                        if let Some(ref mmio) = self.mmio { mmio.ring_doorbell(slot_id, ep_id); }
                    }
                }
            }
        }
    }
}

fn port_speed_from_slot(_slot_id: u8) -> u32 { 3 } // Assume High Speed

fn register_hid_device(dev: HidEndpoint) {
    unsafe {
        for slot in HID_DEVICES.iter_mut() {
            if slot.is_none() {
                *slot = Some(dev);
                return;
            }
        }
    }
}

fn find_hid_buf_phys(slot_id: u8, ep_id: u8) -> u64 {
    unsafe {
        for dev in HID_DEVICES.iter().flatten() {
            if dev.slot_id == slot_id && dev.endpoint_id == ep_id {
                return dev.buf_phys;
            }
        }
    }
    0
}

fn find_hid_buf_size(slot_id: u8, ep_id: u8) -> usize {
    unsafe {
        for dev in HID_DEVICES.iter().flatten() {
            if dev.slot_id == slot_id && dev.endpoint_id == ep_id {
                return dev.buf_size;
            }
        }
    }
    8
}

/// Called when a Transfer Event arrives for a HID device.
fn process_hid_transfer_event(slot_id: u8, ep_id: u8) {
    unsafe {
        for dev in HID_DEVICES.iter().flatten() {
            if dev.slot_id != slot_id || dev.endpoint_id != ep_id { continue; }

            // Read report from buffer (phys addr → virt via HHDM)
            let virt = crate::memory::phys_to_virt(dev.buf_phys);

            if dev.protocol == HID_PROTOCOL_KEYBOARD && dev.buf_size >= 8 {
                let report_ptr = virt as *const HidKeyboardReport;
                let report = core::ptr::read_volatile(report_ptr);
                process_keyboard_report(&report);
            } else if dev.protocol == HID_PROTOCOL_MOUSE && dev.buf_size >= 3 {
                let report_ptr = virt as *const HidMouseReport;
                let report = core::ptr::read_volatile(report_ptr);
                process_mouse_report(&report);
            }
            return;
        }
    }
}

// ===========================================================================
// HID Report Processing – keyboard and mouse
// ===========================================================================

/// Convert a keyboard HID report to PS/2 Set 1 scancodes and push via push_key().
fn process_keyboard_report(report: &HidKeyboardReport) {
    let prev = unsafe { LAST_KEYBOARD_REPORT };

    // Modifier key changes
    for (bit, scancode) in &MODIFIER_SCANCODES {
        let mask = 1u8 << bit;
        let was = (prev.modifiers & mask) != 0;
        let now = (report.modifiers & mask) != 0;
        if now && !was { crate::interrupts::push_key(*scancode); }
        else if !now && was { crate::interrupts::push_key(*scancode | 0x80); }
    }

    // Key presses (new keys that weren't pressed before)
    'outer_press: for &hid in &report.keys {
        if hid == 0 { continue; }
        for &prev_hid in &prev.keys {
            if prev_hid == hid { continue 'outer_press; } // still held
        }
        let sc = HID_TO_PS2[hid as usize];
        if sc != 0 { crate::interrupts::push_key(sc); }
    }

    // Key releases (keys that were pressed but aren't now)
    'outer_rel: for &prev_hid in &prev.keys {
        if prev_hid == 0 { continue; }
        for &hid in &report.keys {
            if hid == prev_hid { continue 'outer_rel; } // still held
        }
        let sc = HID_TO_PS2[prev_hid as usize];
        if sc != 0 { crate::interrupts::push_key(sc | 0x80); }
    }

    unsafe { LAST_KEYBOARD_REPORT = *report; }
}

/// Convert a mouse HID report to a packed mouse packet and push via push_mouse_packet().
/// Packet format: buttons | (dx as u8) << 8 | (dy as u8) << 16
fn process_mouse_report(report: &HidMouseReport) {
    let packet: u32 = (report.buttons as u32)
        | ((report.x as u8 as u32) << 8)
        | ((report.y as u8 as u32) << 16);
    crate::interrupts::push_mouse_packet(packet);
    unsafe { LAST_MOUSE_REPORT = *report; }
}

// ===========================================================================
// Global Controller State and IRQ Handler
// ===========================================================================

pub static mut XHCI: Option<XhciControllerState> = None;

/// USB IRQ handler – processes the XHCI event ring and dispatches HID reports.
fn usb_irq_handler() {
    unsafe {
        let xhci = match XHCI.as_mut() { Some(x) => x, None => return };
        let mmio = match xhci.mmio.as_ref() { Some(m) => m, None => return };

        // Check EINT in USBSTS (offset 0x04)
        let usbsts = mmio.read_operational(0x04);
        if (usbsts & 0x08) == 0 { return; } // not our interrupt
        // Clear EINT (write-1-to-clear)
        mmio.write_operational(0x04, 0x08);

        xhci.process_events();
    }
}

pub fn register_usb_irq_handler(irq: u8) -> Result<(), &'static str> {
    crate::interrupts::set_irq_handler(irq, usb_irq_handler)
}

// ===========================================================================
// Initialization Entry Point
// ===========================================================================

pub fn init() {
    crate::serial::serial_print("[USB-HID] Initializing USB HID driver\n");

    let controllers = detect_usb_controllers();
    if controllers.is_empty() {
        crate::serial::serial_print("[USB-HID] No USB controllers found\n");
        return;
    }

    crate::serial::serial_print(&alloc::format!("[USB-HID] {} USB controller(s) found\n", controllers.len()));

    for ctrl in &controllers {
        match ctrl.controller_type {
            UsbControllerType::XHCI => {
                let state = init_xhci_controller(ctrl);
                if state == UsbControllerState::Ready {
                    // Register IRQ handler for the controller's IRQ line
                    let irq = ctrl.interrupt_line;
                    if irq < 16 {
                        match register_usb_irq_handler(irq) {
                            Ok(_)  => crate::serial::serial_print(&alloc::format!("[USB-HID] IRQ {} handler registered\n", irq)),
                            Err(e) => crate::serial::serial_print(&alloc::format!("[USB-HID] IRQ {} registration failed: {}\n", irq, e)),
                        }
                    }
                    break; // Only one XHCI controller for now
                }
            }
            _ => crate::serial::serial_print(&alloc::format!("[USB-HID] {} not supported\n", ctrl.controller_type.as_str())),
        }
    }

    crate::serial::serial_print("[USB-HID] Initialization complete\n");
}

fn detect_usb_controllers() -> alloc::vec::Vec<UsbController> {
    let mut list = alloc::vec::Vec::new();
    for pci_dev in crate::pci::find_usb_controllers() {
        let controller_type = match pci_dev.prog_if {
            0x00 => UsbControllerType::UHCI,
            0x10 => UsbControllerType::OHCI,
            0x20 => UsbControllerType::EHCI,
            0x30 => UsbControllerType::XHCI,
            _    => continue,
        };
        list.push(UsbController {
            controller_type,
            bus:            pci_dev.bus,
            device:         pci_dev.device,
            function:       pci_dev.function,
            vendor_id:      pci_dev.vendor_id,
            device_id:      pci_dev.device_id,
            bar0:           pci_dev.bar0,
            interrupt_line: pci_dev.interrupt_line,
        });
    }
    list
}

fn init_xhci_controller(ctrl: &UsbController) -> UsbControllerState {
    crate::serial::serial_print(&alloc::format!(
        "[USB-HID] Initializing XHCI at {:02X}:{:02X}.{}\n",
        ctrl.bus, ctrl.device, ctrl.function
    ));

    // Enable PCI memory space + bus master
    unsafe {
        let pci_dev = crate::pci::PciDevice {
            bus: ctrl.bus, device: ctrl.device, function: ctrl.function,
            vendor_id: ctrl.vendor_id, device_id: ctrl.device_id,
            class_code: 0, subclass: 0, prog_if: 0, header_type: 0,
            bar0: ctrl.bar0, interrupt_line: ctrl.interrupt_line,
        };
        crate::pci::enable_device(&pci_dev, true);
    }

    let bar0 = ctrl.bar0 & !0xF;
    if bar0 == 0 {
        crate::serial::serial_print("[USB-HID] Invalid BAR0\n");
        return UsbControllerState::Error;
    }

    let mmio = match XhciMmio::from_bar0(bar0) {
        Ok(m)  => m,
        Err(e) => { crate::serial::serial_print(&alloc::format!("[USB-HID] MMIO map failed: {}\n", e)); return UsbControllerState::Error; }
    };

    let hcsparams1 = mmio.read_capability(0x04);
    let max_slots  = (hcsparams1 & 0xFF) as u8;
    let max_ports  = ((hcsparams1 >> 24) & 0xFF) as u8;

    crate::serial::serial_print(&alloc::format!("[XHCI] MaxSlots={} MaxPorts={}\n", max_slots, max_ports));

    let mut state = XhciControllerState::new(bar0, mmio);
    state.max_slots = max_slots;
    state.max_ports = max_ports;

    if let Err(e) = state.initialize() {
        crate::serial::serial_print(&alloc::format!("[XHCI] initialize() failed: {}\n", e));
        return UsbControllerState::Error;
    }

    if let Err(e) = state.start_controller() {
        crate::serial::serial_print(&alloc::format!("[XHCI] start_controller() failed: {}\n", e));
        return UsbControllerState::Error;
    }

    if let Err(e) = state.enumerate_ports() {
        crate::serial::serial_print(&alloc::format!("[XHCI] enumerate_ports() error: {}\n", e));
        // Non-fatal: controller is still running
    }

    unsafe { XHCI = Some(state); }

    crate::serial::serial_print("[USB-HID] XHCI controller ready\n");
    UsbControllerState::Ready
}
