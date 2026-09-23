// 解析设备树，创建已知的设备并为它们注册中断。
//
// 涉及到中断的设备包括：
//
// - 接收中断的中断控制器
// - 发出中断的设备
//
// 有效的中断控制器应该具有下列三个属性：
//
// - `interrupt-controller`: 指示这是一个中断控制器
// - `interrupt-cells`: 只是要向此控制器注册中断需要几个参数
// - `phandle`: 向此控制器注册中断时使用的一个号码，如果没有设备需要向它注册，可能不存在
//
// 设备注册中断需要 `interrupts-extended` 或 `interrupts` 属性。前者形式为
// `[{phandle, ...,}*]`，即控制器引用和控制器指定数量的参数；后者是同一个
// `interrupt-parent` 下的若干个参数组，没有 phandle。两者都由
// `InterruptsProp` 表示。
//! Probe devices and create drivers from device tree.
//!
//! Specification: <https://github.com/devicetree-org/devicetree-specification/releases/download/v0.3/devicetree-specification-v0.3.pdf>.

use super::IoMapper;
use crate::{
    utils::devicetree::{
        parse_interrupts, parse_reg, Devicetree, InheritProps, InterruptsProp, Node, StringList,
    },
    Device, DeviceError, DeviceResult, PhysAddr, VirtAddr,
};
use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};

const MODULE: &str = "device-tree";

type DevWithInterrupt = (Device, InterruptsProp);

/// 设备树中中断控制器特有的属性
struct IntcProps {
    phandle: u32,
    interrupt_cells: u32,
}

/// 查找表保存的中断控制器信息
struct Intc {
    index: usize,
    cells: usize,
}

/// A builder to probe devices and create drivers from device tree.
pub struct DevicetreeDriverBuilder<M: IoMapper> {
    dt: Devicetree,
    io_mapper: M,
}

impl<M: IoMapper> DevicetreeDriverBuilder<M> {
    /// Prepare to parse DTB from the given virtual address.
    pub fn new(dtb_base_vaddr: VirtAddr, io_mapper: M) -> DeviceResult<Self> {
        Ok(Self {
            dt: Devicetree::from(dtb_base_vaddr)?,
            io_mapper,
        })
    }

    /// Parse the device tree from root, and returns an array of [`Device`] it found.
    pub fn build(&self) -> DeviceResult<Vec<Device>> {
        let mut intc_map = BTreeMap::new(); // phandle -> intc
        let mut dev_list = Vec::new(); // devices

        // 为 d1 启动 uart5
        // 硬编码只是一个临时的方案
        #[cfg(feature = "allwinner")]
        {
            use d1_pac::{ccu::RegisterBlock as Ccu, gpio::RegisterBlock as Gpio, CCU, GPIO};

            let gpio = unsafe { &*(self.mmap(GPIO::PTR as _, 0x1000)? as *const Gpio) };
            let ccu = unsafe { &*(self.mmap(CCU::PTR as _, 0x800)? as *const Ccu) };

            #[rustfmt::skip]
            gpio.pb_cfg0.modify(|r, w| unsafe {
                w.bits(r.bits())
                 .pb0_select().uart2_tx()
                 .pb1_select().uart2_rx()
                 .pb4_select().uart5_tx()
                 .pb5_select().uart5_rx()
            });
            #[rustfmt::skip]
            gpio.pb_cfg1.modify(|r, w| unsafe {
                w.bits(r.bits())
                 .pb8_select().uart0_tx()
                 .pb9_select().uart0_rx()
            });
            #[rustfmt::skip]
            gpio.pd_cfg1.modify(|r, w| unsafe {
                w.bits(r.bits())
                 .pd10_select().uart3_tx()
                 .pd11_select().uart3_rx()
            });
            #[rustfmt::skip]
            ccu.uart_bgr.write(|w| w
                .uart0_rst()   .deassert()
                .uart0_gating().pass()
                .uart2_rst()   .deassert()
                .uart2_gating().pass()
                .uart3_rst()   .deassert()
                .uart3_gating().pass()
                .uart5_rst()   .deassert()
                .uart5_gating().pass()
            );
        }

        // 解析设备树
        self.dt.walk(&mut |node, comp, props| {
            debug!(
                "{MODULE}: parsing node {:?} with compatible {comp:?}",
                node.name
            );
            // parse interrupt controller
            let res = if node.has_prop("interrupt-controller") {
                self.parse_intc(node, comp, props).map(|(dev, intc)| {
                    intc_map.insert(
                        intc.phandle,
                        Intc {
                            index: dev_list.len(),
                            cells: intc.interrupt_cells as _,
                        },
                    );
                    dev
                })
            } else {
                // parse other device
                match comp {
                    #[cfg(feature = "virtio")]
                    c if c.contains("virtio,mmio") => self.parse_virtio(node, props),
                    #[cfg(not(feature = "loopback"))]
                    c if c.contains("allwinner,sunxi-gmac") => {
                        self.parse_ethernet(node, comp, props)
                    }
                    c if c.contains("ns16550a") || c.iter().any(|str| str.ends_with("uart")) => {
                        self.parse_uart(node, comp, props)
                    }
                    _ => Err(DeviceError::NotSupported),
                }
            };
            match res {
                Ok(dev) => dev_list.push(dev),
                Err(DeviceError::NotSupported) => {}
                Err(err) => warn!("{MODULE}: failed to parsing node {:?}: {err:?}", node.name),
            }
        });

        // 注册中断
        for (device, interrupts) in &dev_list {
            register_interrupts(&dev_list, &intc_map, device, interrupts)?;
        }

        // 丢弃中断信息
        Ok(dev_list.into_iter().map(|(dev, _)| dev).collect())
    }

    fn mmap(&self, phys_addr: PhysAddr, len: usize) -> DeviceResult<VirtAddr> {
        self.io_mapper
            .query_or_map(phys_addr, len)
            .ok_or(DeviceError::NoResources)
    }
}

/// Walks one device's interrupt list and registers every specifier in it
/// with the controller it names.
fn register_interrupts(
    dev_list: &[DevWithInterrupt],
    intc_map: &BTreeMap<u32, Intc>,
    device: &Device,
    interrupts: &InterruptsProp,
) -> DeviceResult {
    // 分解 interrupts / interrupts-extended
    let mut rest = interrupts.specs.as_slice();
    while !rest.is_empty() {
        // `interrupts-extended` names a parent in front of every specifier;
        // plain `interrupts` is a list of specifiers that all belong to the
        // one parent, and none of them carries a phandle.
        let (phandle, args_at): (u32, usize) = match interrupts.parent {
            Some(parent) => (parent, 0),
            None => (rest[0], 1),
        };
        let Intc { index, cells } = match intc_map.get(&phandle) {
            Some(intc) => intc,
            None => {
                warn!("{MODULE}: no such node with phandle {phandle:#x} as the interrupt-parent");
                return Err(DeviceError::InvalidParam);
            }
        };
        // `cells` is whatever the controller's `#interrupt-cells` said, so
        // it can be anything at all. Walking off the end of the list used
        // to panic the kernel; and a step of zero would spin here forever.
        let step = match args_at.checked_add(*cells) {
            Some(0) => {
                warn!("{MODULE}: interrupt parent {phandle:#x} asks for no cells at all");
                return Ok(());
            }
            Some(step) => step,
            None => {
                warn!("{MODULE}: interrupt parent {phandle:#x} asks for {cells} cells");
                return Err(DeviceError::InvalidParam);
            }
        };
        let irq_num = match rest.get(args_at..step) {
            // The interrupt number is the first argument of the specifier.
            Some(spec) => spec.first().copied(),
            None => {
                // A tail too short for one more specifier. Everything ahead
                // of it is registered already, so stop at this device
                // rather than refuse the whole device list.
                warn!(
                    "{MODULE}: {device:?} ends mid-specifier: parent {phandle:#x} asks for {cells} cell(s), {} left",
                    rest.len() - args_at.min(rest.len())
                );
                return Ok(());
            }
        };
        rest = &rest[step..];

        let (intc, _) = &dev_list[*index];
        let irq = match intc {
            Device::Irq(irq) => irq,
            _ => {
                warn!("{MODULE}: node with phandle {phandle:#x} is not an interrupt-controller");
                return Err(DeviceError::InvalidParam);
            }
        };
        if let Some(irq_num) = irq_num {
            if irq_num != 0xffff_ffff {
                info!("{MODULE}: register interrupts for {intc:?}: {device:?}, irq_num={irq_num}");
                if irq.register_device(irq_num as _, device.inner()).is_ok() {
                    irq.unmask(irq_num as _)?;
                } else {
                    // Used to be silent, and a device that never gets its
                    // interrupt looks exactly like a device that is not there.
                    warn!("{MODULE}: {intc:?} refused irq_num={irq_num} for {device:?}");
                }
            }
        }
    }
    Ok(())
}

#[allow(dead_code)]
#[allow(unused_imports)]
#[allow(unused_variables)]
#[allow(unreachable_code)]
impl<M: IoMapper> DevicetreeDriverBuilder<M> {
    /// Parse nodes for interrupt controllers.
    fn parse_intc(
        &self,
        node: &Node,
        comp: &StringList,
        props: &InheritProps,
    ) -> DeviceResult<(DevWithInterrupt, IntcProps)> {
        let phandle = node
            .prop_u32("phandle")
            .map_err(|_| DeviceError::InvalidParam)?;
        let interrupt_cells = node
            .prop_u32("#interrupt-cells")
            .map_err(|_| DeviceError::InvalidParam)?;
        let interrupts = parse_interrupts(node, props)?;
        let base_vaddr =
            parse_reg(node, props).and_then(|(paddr, size)| self.mmap(paddr as _, size as _));
        use crate::irq::*;
        let dev = Device::Irq(match comp {
            #[cfg(any(target_arch = "riscv32", target_arch = "riscv64"))]
            c if c.contains("riscv,cpu-intc") => Arc::new(riscv::Intc::new()),
            #[cfg(any(target_arch = "riscv32", target_arch = "riscv64"))]
            c if c.contains("riscv,plic0") => Arc::new(riscv::Plic::new(base_vaddr?)),
            #[cfg(any(target_arch = "riscv32", target_arch = "riscv64"))]
            c if c.contains("sifive,fu540-c000-plic") => Arc::new(riscv::Plic::new(base_vaddr?)),
            _ => return Err(DeviceError::NotSupported),
        });

        Ok((
            (dev, interrupts),
            IntcProps {
                phandle,
                interrupt_cells,
            },
        ))
    }

    /// Parse nodes for virtio devices over MMIO.
    #[cfg(feature = "virtio")]
    fn parse_virtio(&self, node: &Node, props: &InheritProps) -> DeviceResult<DevWithInterrupt> {
        use crate::virtio::*;
        use virtio_drivers::{DeviceType, VirtIOHeader};

        let interrupts = parse_interrupts(node, props)?;
        let base_vaddr =
            parse_reg(node, props).and_then(|(paddr, size)| self.mmap(paddr as _, size as _))?;
        let header = unsafe { &mut *(base_vaddr as *mut VirtIOHeader) };
        if !header.verify() {
            return Err(DeviceError::NotSupported);
        }
        info!(
            "{MODULE}: detected virtio device: vendor_id={:#X}, type={:?}",
            header.vendor_id(),
            header.device_type()
        );

        let dev = match header.device_type() {
            DeviceType::Block => Device::Block(Arc::new(VirtIoBlk::new(header)?)),
            DeviceType::GPU => Device::Display(Arc::new(VirtIoGpu::new(header)?)),
            DeviceType::Input => Device::Input(Arc::new(VirtIoInput::new(header)?)),
            DeviceType::Console => Device::Uart(Arc::new(VirtIoConsole::new(header)?)),
            _ => return Err(DeviceError::NotSupported),
        };

        Ok((dev, interrupts))
    }

    /// Parse nodes for Ethernet devices.
    fn parse_ethernet(
        &self,
        node: &Node,
        comp: &StringList,
        props: &InheritProps,
    ) -> DeviceResult<DevWithInterrupt> {
        let interrupts = parse_interrupts(node, props)?;
        let base_vaddr =
            parse_reg(node, props).and_then(|(paddr, size)| self.mmap(paddr as _, size as _));
        info!("Ethernet gmac init ...");

        // A node that asks for no interrupt at all has no first interrupt
        // number, and reading one out of the empty list panicked the kernel.
        let irq_num = match interrupts.first_irq() {
            Some(irq_num) => irq_num,
            None => {
                warn!("{MODULE}: ethernet node {:?} has no interrupts", node.name);
                return Err(DeviceError::InvalidParam);
            }
        };
        use crate::net::*;
        let dev = Device::Net(match comp {
            #[cfg(target_arch = "riscv64")]
            c if c.contains("allwinner,sunxi-gmac") => {
                Arc::new(rtlx_init(irq_num as usize, |paddr, size| {
                    self.io_mapper.query_or_map(paddr, size)
                })?)
            }
            _ => return Err(DeviceError::NotSupported),
        });

        Ok((dev, interrupts))
    }

    /// Parse nodes for UART devices.
    fn parse_uart(
        &self,
        node: &Node,
        comp: &StringList,
        props: &InheritProps,
    ) -> DeviceResult<DevWithInterrupt> {
        let interrupts = parse_interrupts(node, props)?;
        let base_vaddr =
            parse_reg(node, props).and_then(|(paddr, size)| self.mmap(paddr as _, size as _))?;

        use crate::uart::*;
        let dev = Device::Uart(match comp {
            c if c.contains("ns16550a") => {
                Arc::new(unsafe { Uart16550Mmio::<u8>::new(base_vaddr) })
            }
            c if c.contains("snps,dw-apb-uart") => {
                Arc::new(unsafe { Uart16550Mmio::<u32>::new(base_vaddr) })
            }
            #[cfg(feature = "allwinner")]
            c if c.contains("allwinner,sun20i-uart") => Arc::new(UartAllwinner::new(base_vaddr)),
            #[cfg(feature = "fu740")]
            c if c.contains("sifive,fu740-c000-uart") => {
                Arc::new(unsafe { UartU740Mmio::<u32>::new(base_vaddr) })
            }
            _ => return Err(DeviceError::NotSupported),
        });

        Ok((dev, interrupts))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheme::{IrqHandler, IrqScheme, Scheme};
    use alloc::{
        boxed::Box,
        string::{String, ToString},
        vec,
    };
    use lock::Mutex;

    /// An interrupt controller that only writes down what it was asked to do.
    ///
    /// The real ones are all behind `cfg(target_arch = ...)`, so on the host
    /// there is no way to reach this code with a controller the device tree
    /// walk produced itself.
    struct FakeIntc {
        registered: Mutex<Vec<usize>>,
        unmasked: Mutex<Vec<usize>>,
        /// Numbers `register_device` turns down, the way a real controller
        /// turns down a number outside its range.
        refuse: Vec<usize>,
        unmask_fails: bool,
    }

    impl FakeIntc {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                registered: Mutex::new(Vec::new()),
                unmasked: Mutex::new(Vec::new()),
                refuse: Vec::new(),
                unmask_fails: false,
            })
        }
        fn refusing(irqs: &[usize]) -> Arc<Self> {
            Arc::new(Self {
                registered: Mutex::new(Vec::new()),
                unmasked: Mutex::new(Vec::new()),
                refuse: irqs.to_vec(),
                unmask_fails: false,
            })
        }
        fn with_failing_unmask() -> Arc<Self> {
            Arc::new(Self {
                registered: Mutex::new(Vec::new()),
                unmasked: Mutex::new(Vec::new()),
                refuse: Vec::new(),
                unmask_fails: true,
            })
        }
    }

    impl Scheme for FakeIntc {
        fn name(&self) -> &str {
            "fake-intc"
        }
    }

    impl IrqScheme for FakeIntc {
        fn is_valid_irq(&self, irq_num: usize) -> bool {
            !self.refuse.contains(&irq_num)
        }
        fn mask(&self, _irq_num: usize) -> DeviceResult {
            Ok(())
        }
        fn unmask(&self, irq_num: usize) -> DeviceResult {
            if self.unmask_fails {
                return Err(DeviceError::NoResources);
            }
            self.unmasked.lock().push(irq_num);
            Ok(())
        }
        fn register_handler(&self, irq_num: usize, _handler: IrqHandler) -> DeviceResult {
            if self.refuse.contains(&irq_num) {
                return Err(DeviceError::InvalidParam);
            }
            self.registered.lock().push(irq_num);
            Ok(())
        }
        fn unregister(&self, _irq_num: usize) -> DeviceResult {
            Ok(())
        }
    }

    /// A device that is not an interrupt controller, to stand in for the ones
    /// the walk would produce on a real machine.
    struct NotAnIntc;
    impl Scheme for NotAnIntc {
        fn name(&self) -> &str {
            "not-an-intc"
        }
    }
    impl IrqScheme for NotAnIntc {
        fn is_valid_irq(&self, _irq_num: usize) -> bool {
            false
        }
        fn mask(&self, _irq_num: usize) -> DeviceResult {
            Ok(())
        }
        fn unmask(&self, _irq_num: usize) -> DeviceResult {
            Ok(())
        }
        fn register_handler(&self, _irq_num: usize, _handler: IrqHandler) -> DeviceResult {
            Ok(())
        }
        fn unregister(&self, _irq_num: usize) -> DeviceResult {
            Ok(())
        }
    }

    /// `dev_list[0]` is the controller, `dev_list[1]` the device wired to it.
    fn one_controller(
        intc: Arc<FakeIntc>,
        cells: usize,
        interrupts: InterruptsProp,
    ) -> (Vec<DevWithInterrupt>, BTreeMap<u32, Intc>) {
        let dev_list = vec![
            (Device::Irq(intc), InterruptsProp::default()),
            (Device::Irq(Arc::new(NotAnIntc)), interrupts),
        ];
        let mut intc_map = BTreeMap::new();
        intc_map.insert(7, Intc { index: 0, cells });
        (dev_list, intc_map)
    }

    fn run(dev_list: &[DevWithInterrupt], intc_map: &BTreeMap<u32, Intc>) -> DeviceResult {
        let (device, interrupts) = &dev_list[1];
        register_interrupts(dev_list, intc_map, device, interrupts)
    }

    fn seen(intc: &Arc<FakeIntc>) -> (Vec<usize>, Vec<usize>) {
        (intc.registered.lock().clone(), intc.unmasked.lock().clone())
    }

    // ---- walking the list -------------------------------------------------

    #[test]
    fn a_device_with_three_interrupts_registers_all_three() {
        let intc = FakeIntc::new();
        // `interrupts = <35 36 37>` under a controller of one cell each: three
        // interrupts for one device, which is an ordinary thing for an MMC or
        // an Ethernet controller to ask for. Flattening the list by putting
        // the phandle in front once made cell 36 look like a second phandle,
        // which nothing answers to, and the whole device list was refused.
        let (dev_list, intc_map) = one_controller(
            intc.clone(),
            1,
            InterruptsProp {
                parent: Some(7),
                specs: vec![35, 36, 37],
            },
        );
        assert_eq!(run(&dev_list, &intc_map), Ok(()));
        assert_eq!(seen(&intc), (vec![35, 36, 37], vec![35, 36, 37]));
    }

    #[test]
    fn a_specifier_is_as_long_as_its_controller_says() {
        let intc = FakeIntc::new();
        // Three cells per specifier, first one the number: two interrupts.
        let (dev_list, intc_map) = one_controller(
            intc.clone(),
            3,
            InterruptsProp {
                parent: Some(7),
                specs: vec![35, 0, 4, 36, 0, 4],
            },
        );
        assert_eq!(run(&dev_list, &intc_map), Ok(()));
        assert_eq!(seen(&intc).0, vec![35, 36]);
    }

    #[test]
    fn an_extended_list_names_a_parent_before_every_specifier() {
        let intc = FakeIntc::new();
        let (dev_list, intc_map) = one_controller(
            intc.clone(),
            1,
            InterruptsProp {
                parent: None,
                specs: vec![7, 35, 7, 36],
            },
        );
        assert_eq!(run(&dev_list, &intc_map), Ok(()));
        assert_eq!(seen(&intc).0, vec![35, 36]);
    }

    #[test]
    fn an_extended_list_can_name_a_different_parent_each_time() {
        let first = FakeIntc::new();
        let second = FakeIntc::new();
        let dev_list = vec![
            (Device::Irq(first.clone()), InterruptsProp::default()),
            (Device::Irq(second.clone()), InterruptsProp::default()),
            (
                Device::Irq(Arc::new(NotAnIntc)),
                InterruptsProp {
                    parent: None,
                    specs: vec![7, 35, 9, 4],
                },
            ),
        ];
        let mut intc_map = BTreeMap::new();
        intc_map.insert(7, Intc { index: 0, cells: 1 });
        intc_map.insert(9, Intc { index: 1, cells: 1 });
        let (device, interrupts) = &dev_list[2];
        assert_eq!(
            register_interrupts(&dev_list, &intc_map, device, interrupts),
            Ok(())
        );
        assert_eq!(seen(&first).0, vec![35]);
        assert_eq!(seen(&second).0, vec![4]);
    }

    #[test]
    fn a_list_that_stops_mid_specifier_keeps_what_came_before_it() {
        let intc = FakeIntc::new();
        // Two cells per specifier and five cells of list: the last one is not
        // a specifier. Slicing past the end panicked the kernel.
        let (dev_list, intc_map) = one_controller(
            intc.clone(),
            2,
            InterruptsProp {
                parent: Some(7),
                specs: vec![35, 4, 36, 4, 37],
            },
        );
        assert_eq!(run(&dev_list, &intc_map), Ok(()));
        assert_eq!(seen(&intc).0, vec![35, 36]);
    }

    #[test]
    fn a_list_too_short_for_even_one_specifier_registers_nothing() {
        let intc = FakeIntc::new();
        let (dev_list, intc_map) = one_controller(
            intc.clone(),
            3,
            InterruptsProp {
                parent: None,
                specs: vec![7, 35],
            },
        );
        assert_eq!(run(&dev_list, &intc_map), Ok(()));
        assert_eq!(seen(&intc).0, Vec::<usize>::new());
    }

    #[test]
    fn a_controller_that_asks_for_no_cells_at_all_does_not_spin() {
        let intc = FakeIntc::new();
        // `#interrupt-cells = <0>` with the plain form leaves nothing to step
        // over, so the walk would never reach the end of the list.
        let (dev_list, intc_map) = one_controller(
            intc.clone(),
            0,
            InterruptsProp {
                parent: Some(7),
                specs: vec![35, 36],
            },
        );
        assert_eq!(run(&dev_list, &intc_map), Ok(()));
        assert_eq!(seen(&intc).0, Vec::<usize>::new());
    }

    #[test]
    fn a_device_that_asks_for_nothing_is_left_alone() {
        let intc = FakeIntc::new();
        let (dev_list, intc_map) = one_controller(intc.clone(), 1, InterruptsProp::default());
        assert_eq!(run(&dev_list, &intc_map), Ok(()));
        assert_eq!(seen(&intc).0, Vec::<usize>::new());
    }

    // ---- what stops the walk ----------------------------------------------

    #[test]
    fn an_unknown_interrupt_parent_is_an_error() {
        let intc = FakeIntc::new();
        let (dev_list, intc_map) = one_controller(
            intc,
            1,
            InterruptsProp {
                parent: Some(42),
                specs: vec![35],
            },
        );
        assert_eq!(run(&dev_list, &intc_map), Err(DeviceError::InvalidParam));
    }

    #[test]
    fn a_parent_that_is_not_an_interrupt_controller_is_an_error() {
        let ram = RamMapper::new();
        let dev_list = vec![
            (
                Device::Uart(Arc::new(unsafe {
                    crate::uart::Uart16550Mmio::<u8>::new(ram.0)
                })),
                InterruptsProp::default(),
            ),
            (
                Device::Irq(Arc::new(NotAnIntc)),
                InterruptsProp {
                    parent: Some(7),
                    specs: vec![35],
                },
            ),
        ];
        let mut intc_map = BTreeMap::new();
        intc_map.insert(7, Intc { index: 0, cells: 1 });
        assert_eq!(run(&dev_list, &intc_map), Err(DeviceError::InvalidParam));
    }

    #[test]
    fn an_interrupt_number_of_all_ones_is_skipped_and_the_walk_goes_on() {
        let intc = FakeIntc::new();
        // A disabled entry, which is how a device tree says "this one is not
        // wired up" without shortening the list.
        let (dev_list, intc_map) = one_controller(
            intc.clone(),
            1,
            InterruptsProp {
                parent: Some(7),
                specs: vec![0xffff_ffff, 36],
            },
        );
        assert_eq!(run(&dev_list, &intc_map), Ok(()));
        assert_eq!(seen(&intc).0, vec![36]);
    }

    #[test]
    fn a_number_the_controller_turns_down_is_not_unmasked() {
        let intc = FakeIntc::refusing(&[35]);
        let (dev_list, intc_map) = one_controller(
            intc.clone(),
            1,
            InterruptsProp {
                parent: Some(7),
                specs: vec![35, 36],
            },
        );
        // Turning one down is not fatal, and the next one still goes in.
        assert_eq!(run(&dev_list, &intc_map), Ok(()));
        assert_eq!(seen(&intc), (vec![36], vec![36]));
    }

    #[test]
    fn an_unmask_that_fails_stops_the_whole_thing() {
        let intc = FakeIntc::with_failing_unmask();
        let (dev_list, intc_map) = one_controller(
            intc.clone(),
            1,
            InterruptsProp {
                parent: Some(7),
                specs: vec![35, 36],
            },
        );
        assert_eq!(run(&dev_list, &intc_map), Err(DeviceError::NoResources));
        assert_eq!(seen(&intc).0, vec![35]);
    }

    // ---- through `build` --------------------------------------------------

    /// Hands out a page of ordinary memory for whatever the tree asks to map,
    /// so a driver that writes its registers at construction writes here.
    struct RamMapper(usize);
    impl RamMapper {
        fn new() -> Self {
            let page: Box<[u64; 512]> = Box::new([0; 512]);
            Self(Box::leak(page).as_ptr() as usize)
        }
    }
    impl IoMapper for RamMapper {
        fn query_or_map(&self, _paddr: PhysAddr, _size: usize) -> Option<VirtAddr> {
            Some(self.0)
        }
    }

    fn cells(v: &[u32]) -> Vec<u8> {
        let mut out = Vec::new();
        for c in v {
            out.extend_from_slice(&c.to_be_bytes());
        }
        out
    }

    fn strs(v: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        for s in v {
            out.extend_from_slice(s.as_bytes());
            out.push(0);
        }
        out
    }

    fn prop(name: &str, value: Vec<u8>) -> (String, Vec<u8>) {
        (name.to_string(), value)
    }

    fn leaf(name: &str, props: Vec<(String, Vec<u8>)>) -> Node {
        Node {
            name: name.to_string(),
            props,
            children: Vec::new(),
        }
    }

    fn soc(children: Vec<Node>) -> Node {
        Node {
            name: String::new(),
            props: vec![
                prop("#address-cells", cells(&[1])),
                prop("#size-cells", cells(&[1])),
            ],
            children,
        }
    }

    fn build(root: Node) -> DeviceResult<Vec<Device>> {
        DevicetreeDriverBuilder {
            dt: Devicetree::from_root(root),
            io_mapper: RamMapper::new(),
        }
        .build()
    }

    #[test]
    fn an_ethernet_node_with_no_interrupts_is_an_error_not_a_panic() {
        // `parse_ethernet` read the first interrupt number straight out of the
        // list, and a node that names none has an empty one.
        let root = soc(vec![leaf(
            "ethernet@4500000",
            vec![
                prop("compatible", strs(&["allwinner,sunxi-gmac"])),
                prop("reg", cells(&[0x0450_0000, 0x1_0000])),
            ],
        )]);
        assert_eq!(build(root).unwrap().len(), 0);
    }

    #[test]
    fn a_uart_node_comes_back_as_a_uart() {
        let root = soc(vec![leaf(
            "serial@2500000",
            vec![
                prop("compatible", strs(&["ns16550a"])),
                prop("reg", cells(&[0x0250_0000, 0x400])),
            ],
        )]);
        let devs = build(root).unwrap();
        assert_eq!(devs.len(), 1);
        assert!(matches!(devs[0], Device::Uart(_)));
    }

    #[test]
    fn a_uart_is_also_recognised_by_the_end_of_its_compatible_string() {
        // `snps,dw-apb-uart` is the D1's, and nothing in it says 16550.
        let root = soc(vec![leaf(
            "serial@2500000",
            vec![
                prop("compatible", strs(&["snps,dw-apb-uart"])),
                prop("reg", cells(&[0x0250_0000, 0x400])),
            ],
        )]);
        let devs = build(root).unwrap();
        assert_eq!(devs.len(), 1);
        assert!(matches!(devs[0], Device::Uart(_)));
    }

    #[test]
    fn a_node_that_fails_to_parse_does_not_take_the_others_with_it() {
        let root = soc(vec![
            // No `reg`, so there is nothing to map.
            leaf(
                "serial@2500000",
                vec![prop("compatible", strs(&["ns16550a"]))],
            ),
            leaf(
                "serial@2500400",
                vec![
                    prop("compatible", strs(&["ns16550a"])),
                    prop("reg", cells(&[0x0250_0400, 0x400])),
                ],
            ),
            // Nothing here knows what this is.
            leaf(
                "rtc@2000000",
                vec![
                    prop("compatible", strs(&["allwinner,sun6i-a31-rtc"])),
                    prop("reg", cells(&[0x0200_0000, 0x400])),
                ],
            ),
        ]);
        assert_eq!(build(root).unwrap().len(), 1);
    }

    #[test]
    fn a_device_whose_interrupt_parent_was_never_parsed_stops_the_build() {
        // Every interrupt controller this builder knows how to make is behind
        // `cfg(target_arch = "riscv...")`, so on the host the walk finds none
        // and a node that names one is left pointing at nothing.
        let root = soc(vec![
            leaf(
                "interrupt-controller@c000000",
                vec![
                    prop("compatible", strs(&["riscv,plic0"])),
                    prop("interrupt-controller", Vec::new()),
                    prop("phandle", cells(&[7])),
                    prop("#interrupt-cells", cells(&[1])),
                    prop("reg", cells(&[0x0c00_0000, 0x40_0000])),
                ],
            ),
            leaf(
                "serial@2500000",
                vec![
                    prop("compatible", strs(&["ns16550a"])),
                    prop("reg", cells(&[0x0250_0000, 0x400])),
                    prop("interrupt-parent", cells(&[7])),
                    prop("interrupts", cells(&[35])),
                ],
            ),
        ]);
        assert_eq!(build(root).unwrap_err(), DeviceError::InvalidParam);
    }
}
