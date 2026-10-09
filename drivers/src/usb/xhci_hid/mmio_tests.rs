use super::*;

/// A BAR in host memory, with slack behind it so a write past the end lands
/// somewhere a test can look at instead of somewhere it must not touch.
struct FakeBar {
    _mem: alloc::vec::Vec<u32>,
    base: usize,
    size: usize,
}

impl FakeBar {
    /// `size` bytes of BAR plus `size` bytes of slack behind it.
    fn new(size: usize) -> Self {
        let mut mem = alloc::vec![0u32; size / 2];
        let base = mem.as_mut_ptr() as usize;
        let mut bar = Self {
            _mem: mem,
            base,
            size,
        };
        bar.set(0, 0x20); // CAPLENGTH
        bar
    }

    fn set(&mut self, off: usize, v: u32) {
        unsafe { write_volatile((self.base + off) as *mut u32, v) }
    }

    fn get(&self, off: usize) -> u32 {
        unsafe { read_volatile((self.base + off) as *const u32) }
    }

    fn mmio(&self) -> DeviceResult<XhciMmio> {
        XhciMmio::from_virt(self.base, self.size)
    }
}

/// The bug. USBLEGCTLSTS is four bytes at `cap_ptr + 4`, so a legacy
/// capability header in the last dword of the BAR has no room for it -- and
/// the write went out of the window anyway.
#[test]
fn a_legacy_capability_in_the_last_dword_of_the_bar_is_not_written_past_it() {
    let size = 0x40;
    let mut bar = FakeBar::new(size);
    bar.set(0x10, 0x000f_0000); // HCCPARAMS1: xECP en el dword 0x0f -> byte 0x3c
    bar.set(0x3c, 0x0000_0001); // capacidad 1 (USB Legacy Support), sin siguiente
    bar.set(size, 0xa5a5_a5a5); // centinela justo detras de la BAR
    bar.set(size + 4, 0x5a5a_5a5a);
    bar.mmio().expect("una BAR valida").perform_bios_handoff();
    assert_eq!(
        bar.get(size),
        0xa5a5_a5a5,
        "el handoff ha escrito USBLEGCTLSTS cuatro bytes fuera de la BAR"
    );
    assert_eq!(bar.get(size + 4), 0x5a5a_5a5a, "y ocho bytes fuera");
}

#[test]
fn a_legacy_capability_with_room_for_its_register_does_get_written() {
    let size = 0x40;
    let mut bar = FakeBar::new(size);
    bar.set(0x10, 0x000e_0000); // xECP en el dword 0x0e -> byte 0x38
    bar.set(0x38, 0x0000_0001);
    bar.set(size, 0xa5a5_a5a5);
    bar.mmio().expect("una BAR valida").perform_bios_handoff();
    assert_eq!(
        bar.get(0x3c),
        0xffff_0000,
        "USBLEGCTLSTS no se ha escrito estando dentro de la BAR"
    );
    assert_eq!(bar.get(size), 0xa5a5_a5a5, "y aun asi se ha salido");
}

#[test]
fn a_register_window_with_no_room_for_one_register_is_refused() {
    let size = 0x40;
    for (name, off, value) in [
        ("CAPLENGTH", 0x00, size as u32),
        ("DBOFF", 0x14, size as u32),
        ("RTSOFF", 0x18, size as u32),
    ] {
        let mut bar = FakeBar::new(size);
        bar.set(off, value);
        assert!(
            bar.mmio().is_err(),
            "una BAR de {} bytes con {} justo en el final se acepta",
            size,
            name
        );
        let mut bar = FakeBar::new(size);
        bar.set(off, value - 4);
        assert!(
            bar.mmio().is_ok(),
            "una BAR de {} bytes con {} a un registro del final se rechaza",
            size,
            name
        );
    }
    assert!(
        XhciMmio::from_virt(0, size).is_err(),
        "una BAR en la direccion cero se acepta"
    );
}

/// A capability chain longer than [`XHCI_MAX_XECP_TRAVERSAL`]: the guard has
/// to stop the walk, so a legacy capability sitting past the 256th link is
/// never reached. `cap_ptr` only ever grows, so this -- and not a cycle --
/// is what a chain that runs away actually looks like.
#[test]
fn a_chain_longer_than_the_traversal_guard_stops_at_the_guard() {
    let size = 0x500;
    let mut bar = FakeBar::new(size);
    // La cadena arranca en el dword 8 (byte 0x20) y cada eslabon apunta al
    // siguiente dword: capacidad 2 (no es la legacy), next = 1.
    bar.set(0x10, 0x0008_0000);
    for dw in 8..320 {
        bar.set(dw * 4, 0x0000_0102);
    }
    // La legacy, mas alla del eslabon 256 (dword 8 + 256 = 264).
    let legacy_dw = 270;
    bar.set(legacy_dw * 4, 0x0000_0001);
    bar.set((legacy_dw + 1) * 4, 0xa5a5_a5a5);
    bar.mmio().expect("una BAR valida").perform_bios_handoff();
    assert_eq!(
        bar.get((legacy_dw + 1) * 4),
        0xa5a5_a5a5,
        "la guarda de {} eslabones no ha cortado el recorrido de la cadena xECP",
        XHCI_MAX_XECP_TRAVERSAL
    );
}

/// A chain that simply ends: `next == 0` is the terminator.
#[test]
fn a_chain_with_no_legacy_capability_writes_nothing() {
    let size = 0x100;
    let mut bar = FakeBar::new(size);
    bar.set(0x10, 0x0010_0000); // xECP en el dword 0x10 -> byte 0x40
                                // Capacidad 2 (no es la legacy), con `next` = 0 dwords: se apunta a si
                                // misma, porque `cap_ptr += next << 2` no avanza.
    bar.set(0x40, 0x0000_0002);
    bar.set(0x44, 0xa5a5_a5a5);
    bar.mmio().expect("una BAR valida").perform_bios_handoff();
    assert_eq!(
        bar.get(0x44),
        0xa5a5_a5a5,
        "una cadena sin capacidad legacy ha escrito USBLEGCTLSTS"
    );
}
