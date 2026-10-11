// 封装对外设地址空间的访问，包括内存映射 IO 和端口映射 IO。
//
// 要了解这两种访问外设的方式，查看[维基百科](https://en.wikipedia.org/wiki/Memory-mapped_I/O)。
//! Peripheral address space access, including memory-mapped IO and port-mapped IO.
//!
//! About these two methods of performing I/O, see [wikipedia](https://en.wikipedia.org/wiki/Memory-mapped_I/O).

use core::ops::{BitAnd, BitOr, Not};

mod mmio;
#[cfg(target_arch = "x86_64")]
mod pmio;

pub use mmio::Mmio;
#[cfg(target_arch = "x86_64")]
pub use pmio::Pmio;

// 用于处理外设地址空间访问的接口。
/// An interface for dealing with device address space access.
pub trait Io {
    // 可访问的对象的类型。
    /// The type of object to access.
    type Value: Copy
        + BitAnd<Output = Self::Value>
        + BitOr<Output = Self::Value>
        + Not<Output = Self::Value>;

    // 从外设读取值。
    /// Reads value from device.
    fn read(&self) -> Self::Value;

    // 向外设写入值。
    /// Writes `value` to device.
    fn write(&mut self, value: Self::Value);
}

// 外设地址空间的一个只读单元。
/// A readonly unit in device address space.
#[repr(transparent)]
pub struct ReadOnly<I>(I);

impl<I> ReadOnly<I> {
    // 构造外设地址空间的一个只读单元。
    /// Constructs a readonly unit in device address space.
    pub const fn new(inner: I) -> Self {
        Self(inner)
    }
}

impl<I: Io> ReadOnly<I> {
    // 从外设读取值。
    /// Reads value from device.
    #[inline(always)]
    pub fn read(&self) -> I::Value {
        self.0.read()
    }
}

impl<I> ReadOnly<I> {
    /// The wrapped unit, for a test that needs to ask *where* the register is
    /// rather than read it. `&I` cannot write (`Io::write` takes `&mut self`),
    /// so this does not weaken the wrapper; it is `#[cfg(test)]` all the same.
    #[cfg(test)]
    pub(crate) fn unit(&self) -> &I {
        &self.0
    }
}

// 外设地址空间的一个只写单元。
/// A write-only unit in device address space.
#[repr(transparent)]
pub struct WriteOnly<I>(I);

impl<I> WriteOnly<I> {
    // 构造外设地址空间的一个只写单元。
    /// Constructs a write-only unit in device address space.
    pub const fn new(inner: I) -> Self {
        Self(inner)
    }
}

impl<I: Io> WriteOnly<I> {
    // 向外设写入值。
    /// Writes `value` to device.
    #[inline(always)]
    pub fn write(&mut self, value: I::Value) {
        self.0.write(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::Cell;

    /// A unit of "device address space" that is a cell, so what the wrappers
    /// did to it can be read back. `Io::write` takes `&mut self`, so the cell
    /// is only here to let a read-only wrapper be inspected too.
    struct Fake<'a> {
        value: &'a Cell<u32>,
        reads: &'a Cell<usize>,
        writes: &'a Cell<usize>,
    }

    impl Io for Fake<'_> {
        type Value = u32;

        fn read(&self) -> u32 {
            self.reads.set(self.reads.get() + 1);
            self.value.get()
        }

        fn write(&mut self, value: u32) {
            self.writes.set(self.writes.get() + 1);
            self.value.set(value);
        }
    }

    struct Bench {
        value: Cell<u32>,
        reads: Cell<usize>,
        writes: Cell<usize>,
    }

    impl Bench {
        fn new(initial: u32) -> Self {
            Self {
                value: Cell::new(initial),
                reads: Cell::new(0),
                writes: Cell::new(0),
            }
        }

        fn unit(&self) -> Fake<'_> {
            Fake {
                value: &self.value,
                reads: &self.reads,
                writes: &self.writes,
            }
        }
    }

    /// A read-only register must read through to the device, once per call --
    /// a wrapper that cached would hide a status bit changing underneath, and
    /// one that read twice would double every poll's bus traffic.
    #[test]
    fn a_readonly_register_reads_through_once_per_call() {
        let bench = Bench::new(0x1234_5678);
        let reg = ReadOnly::new(bench.unit());
        assert_eq!(reg.read(), 0x1234_5678);
        assert_eq!(bench.reads.get(), 1);
        bench.value.set(0xffff_0000);
        assert_eq!(reg.read(), 0xffff_0000);
        assert_eq!(bench.reads.get(), 2);
        assert_eq!(bench.writes.get(), 0, "a read-only register wrote");
    }

    /// A write-only register must write through, once, with the value given:
    /// a read-modify-write here would read a register the hardware does not
    /// allow reading, which on some devices returns garbage and on others
    /// hangs the bus.
    #[test]
    fn a_writeonly_register_writes_through_once_per_call() {
        let bench = Bench::new(0);
        let mut reg = WriteOnly::new(bench.unit());
        reg.write(0xdead_beef);
        assert_eq!(bench.value.get(), 0xdead_beef);
        assert_eq!(bench.writes.get(), 1);
        assert_eq!(bench.reads.get(), 0, "a write-only register read back");
        reg.write(0);
        assert_eq!(bench.value.get(), 0);
        assert_eq!(bench.writes.get(), 2);
    }

    /// Both wrappers are `#[repr(transparent)]` and constructed in `const`
    /// context, because a register map is a `const` table of them. A wrapper
    /// that grew a field or lost `const` would change every driver's layout.
    #[test]
    fn the_wrappers_are_transparent_and_const_constructible() {
        const CELL: Cell<u32> = Cell::new(0);
        // `const fn new` is the property under test: this would not compile
        // otherwise.
        const RO: ReadOnly<u32> = ReadOnly::new(7);
        const WO: WriteOnly<u32> = WriteOnly::new(7);
        let _ = CELL;
        assert_eq!(core::mem::size_of_val(&RO), core::mem::size_of::<u32>());
        assert_eq!(core::mem::size_of_val(&WO), core::mem::size_of::<u32>());
        assert_eq!(
            core::mem::size_of::<ReadOnly<Fake<'_>>>(),
            core::mem::size_of::<Fake<'_>>()
        );
    }

    /// `unit()` exists so a test can ask WHERE a register is without reading
    /// it; it must hand back the same unit and must not touch the device.
    #[test]
    fn asking_a_readonly_register_for_its_unit_does_not_touch_the_device() {
        let bench = Bench::new(9);
        let reg = ReadOnly::new(bench.unit());
        let unit = reg.unit();
        assert_eq!(bench.reads.get(), 0);
        // And the unit is the live one, not a copy of its value.
        bench.value.set(11);
        assert_eq!(unit.read(), 11);
    }
}
