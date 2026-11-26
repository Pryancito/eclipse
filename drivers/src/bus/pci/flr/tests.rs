use super::*;
use core::sync::atomic::{AtomicU64, Ordering};
use lock::Mutex;

const CONFIG_ADDRESS: u32 = 0x0CF8;
const CONFIG_DATA: u32 = 0x0CFC;

/// 256 bytes of one function's configuration space behind 0xCF8/0xCFC.
struct Fake {
    space: Mutex<[u32; 64]>,
    latched: Mutex<u32>,
}

impl Fake {
    /// A function with the PCI Express capability at 0x60, behind a power
    /// management one at 0x40; `devcap` its Device Capabilities.
    fn with_pcie(devcap: u32) -> Self {
        let mut s = [0u32; 64];
        s[0] = 0x1f06_10de;
        s[1] = (STATUS_CAP_LIST as u32) << 16;
        s[0x34 / 4] = 0x40;
        s[0x40 / 4] = 0x6001; // PM, next 0x60
        s[0x60 / 4] = 0x0010; // PCIe, last
        s[0x64 / 4] = devcap;
        Self {
            space: Mutex::new(s),
            latched: Mutex::new(0),
        }
    }

    fn dword(&self, off: usize) -> u32 {
        self.space.lock()[off / 4]
    }
}

impl PortOps for Fake {
    unsafe fn read8(&self, _: u16) -> u8 {
        unreachable!()
    }
    unsafe fn read16(&self, _: u16) -> u16 {
        unreachable!()
    }
    unsafe fn read32(&self, port: u32) -> u32 {
        assert_eq!(port, CONFIG_DATA);
        self.dword((*self.latched.lock() & 0xFC) as usize)
    }
    unsafe fn write8(&self, _: u16, _: u8) {
        unreachable!()
    }
    unsafe fn write16(&self, _: u16, _: u16) {
        unreachable!()
    }
    unsafe fn write32(&self, port: u32, val: u32) {
        if port == CONFIG_ADDRESS {
            *self.latched.lock() = val;
            return;
        }
        let off = (*self.latched.lock() & 0xFC) as usize;
        self.space.lock()[off / 4] = val;
    }
}

const LOC: Location = Location {
    bus: 0x65,
    device: 0,
    function: 0,
};

/// A clock that moves 1 ms each time it is read.
fn clock() -> impl Fn() -> u64 {
    let t = AtomicU64::new(0);
    move || t.fetch_add(1_000, Ordering::Relaxed)
}

#[test]
fn a_function_that_can_flr_is_told_to() {
    let f = Fake::with_pcie(DEVCAP_FLR);
    assert_eq!(
        unsafe { pcie_cap(&f, CSpaceAccessMethod::IO, LOC) },
        Some(0x60)
    );
    assert!(unsafe { flr(&f, CSpaceAccessMethod::IO, LOC, clock()) });
    assert_eq!(
        f.dword(0x68) & DEVCTL_FLR as u32,
        DEVCTL_FLR as u32,
        "Initiate FLR in Device Control"
    );
    assert_eq!(f.dword(0x68) >> 16, 0, "Device Status left as it was");
}

#[test]
fn a_function_that_cannot_or_is_gone_is_left_alone() {
    let f = Fake::with_pcie(0);
    assert!(!unsafe { flr(&f, CSpaceAccessMethod::IO, LOC, clock()) });
    assert_eq!(f.dword(0x68), 0);

    let gone = Fake::with_pcie(DEVCAP_FLR);
    gone.space.lock()[0] = u32::MAX;
    assert!(!unsafe { flr(&gone, CSpaceAccessMethod::IO, LOC, clock()) });

    let plain = Fake::with_pcie(DEVCAP_FLR);
    plain.space.lock()[1] = 0;
    assert_eq!(
        unsafe { pcie_cap(&plain, CSpaceAccessMethod::IO, LOC) },
        None
    );
    assert!(!unsafe { flr(&plain, CSpaceAccessMethod::IO, LOC, clock()) });
}

/// Transactions that never drain are waited for 100 ms, not for ever.
#[test]
fn pending_transactions_are_waited_for_a_bounded_time() {
    let f = Fake::with_pcie(DEVCAP_FLR);
    f.space.lock()[0x68 / 4] = (DEVSTA_PENDING as u32) << 16;
    assert!(unsafe { flr(&f, CSpaceAccessMethod::IO, LOC, clock()) });
}

/// A capability list that loops is given up on.
#[test]
fn a_list_that_loops_ends() {
    let f = Fake::with_pcie(DEVCAP_FLR);
    f.space.lock()[0x40 / 4] = 0x4001; // PM, next itself
    assert_eq!(unsafe { pcie_cap(&f, CSpaceAccessMethod::IO, LOC) }, None);
}
