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
use alloc::{collections::BTreeMap, string::String, sync::Arc, vec::Vec};

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
    /// Controllers that were already brought up before the walk, by the
    /// `compatible` of the node that describes them. See
    /// [`DevicetreeDriverBuilder::adopt_intc`].
    adopted_intc: Vec<(String, Arc<dyn crate::scheme::IrqScheme>)>,
}

impl<M: IoMapper> DevicetreeDriverBuilder<M> {
    /// Prepare to parse DTB from the given virtual address.
    pub fn new(dtb_base_vaddr: VirtAddr, io_mapper: M) -> DeviceResult<Self> {
        Ok(Self {
            dt: Devicetree::from(dtb_base_vaddr)?,
            io_mapper,
            adopted_intc: Vec::new(),
        })
    }

    /// Hands the walk a controller that is **already running**, to be used for
    /// the node whose `compatible` names it instead of building a new one.
    ///
    /// Without this, a tree that describes a controller the kernel has already
    /// brought up would get a second driver for the same silicon, and the
    /// second one's bring-up would undo the first's: the GIC-400's `init`
    /// disables every SPI and rewrites the priorities, so the timer and the
    /// console would go quiet the moment the device tree was walked. Which is
    /// exactly the position aarch64 is in --- `init_early` sets up the GIC
    /// from the bases in `KCONFIG`, long before there is a tree to read.
    ///
    /// What is **not** checked is that the node really describes the
    /// controller handed over: there is no way to ask an `IrqScheme` what
    /// address it was built with. The node's `reg` is logged so that a human
    /// reading the boot can compare the two.
    pub fn adopt_intc(mut self, compatible: &str, irq: Arc<dyn crate::scheme::IrqScheme>) -> Self {
        self.adopted_intc.push((String::from(compatible), irq));
        self
    }

    /// The controller handed over for this node, if any of its `compatible`
    /// strings names one.
    fn adopted(&self, comp: &StringList) -> Option<Arc<dyn crate::scheme::IrqScheme>> {
        self.adopted_intc
            .iter()
            .find(|(c, _)| comp.contains(c))
            .map(|(_, irq)| irq.clone())
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
                    c if is_ethernet(c) => self.parse_ethernet(node, comp, props),
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

/// Cuales de los `compatible` de un nodo van al constructor de Ethernet.
///
/// Es una funcion y no una lista dentro del `match` de [`DevicetreeDriverBuilder::build`]
/// porque **el `match` de fuera decide si `parse_ethernet` llega a correr**, y
/// las ramas de dentro de `parse_ethernet` no lo dicen: el GENET entro con su
/// rama ahi dentro y con el nodo de la Pi 4 delante nunca se llamaba, porque
/// aqui solo estaba el GMAC del D1. Un driver asi no da ningun error: no se
/// sonda, y el arbol parece no traerlo.
///
/// Con la escotilla `loopback` puesta no hay sonda de Ethernet y nadie la
/// llama, pero los tests siguen comprobandola.
#[cfg(any(not(feature = "loopback"), test))]
fn is_ethernet(comp: &StringList) -> bool {
    // El GMAC del Allwinner D1 (riscv64), el GENET v5 del BCM2711 (la
    // Raspberry Pi 4) y el Cadence GEM del RP1 (la Raspberry Pi 5). Cada uno
    // tiene su rama en `parse_ethernet`, detras del `target_arch` que lo
    // compila.
    //
    // Del GEM se mira el `cdns,macb` y no el `raspberrypi,rp1-gem`, que es el
    // primero de la lista del nodo: el `rp1-gem` es el nombre de la
    // integracion y el `cdns,macb` el del silicio, y es el segundo el que dice
    // que este driver sabe manejarlo. Un GEM del mismo IP en otra placa
    // declararia el segundo y no el primero.
    comp.contains("allwinner,sunxi-gmac")
        || comp.contains("brcm,bcm2711-genet-v5")
        || comp.contains("cdns,macb")
}

/// The interrupt number one specifier names, as the controller's own driver
/// counts them.
///
/// **It is not always the first cell.** How a specifier reads is part of the
/// controller's binding, and `#interrupt-cells` is what tells the two shapes
/// Eclipse has apart:
///
/// - **one cell** is the number itself. That is the RISC-V PLIC and the
///   `riscv,cpu-intc`, which is all that reached this code until now.
/// - **two cells** are `<number flags>`, which is the shape a controller that
///   needs a trigger type but has one interrupt bank uses --- the RP1 of the
///   Raspberry Pi 5 among them (`#interrupt-cells = <2>`, and its
///   `interrupts = <RP1_INT_ETH IRQ_TYPE_LEVEL_HIGH>`). The number is the
///   first cell and needs no offset, because such a controller numbers its own
///   lines from zero. The **second cell is checked against the `IRQ_TYPE_*`
///   values** of `dt-bindings/interrupt-controller/irq.h`: those six are all
///   the legal ones, so a two-cell specifier whose flags cell is something
///   else is a binding of another shape, and guessing it would wire the device
///   to the wrong line.
/// - **three cells** are `<type number flags>`, which is the ARM GIC. The
///   first cell is the TYPE, 0 for an SPI and 1 for a PPI, and taking it for
///   the number would register every shared interrupt in the tree as IRQ 0.
///   The number in the specifier is relative to its bank, and the GIC's own
///   IDs are absolute: an SPI starts at 32 and a PPI at 16, which is the
///   offset `irq_of_parse_and_map` adds in Linux.
///
/// Anything else is a controller whose binding nobody here has read, and a
/// guess would silently wire the device to the wrong line. `None` says so.
fn irq_number(cells: usize, spec: &[u32]) -> Option<u32> {
    match (cells, spec) {
        (1, [number, ..]) => Some(*number),
        (2, [number, flags, ..]) => {
            // `IRQ_TYPE_NONE`, `EDGE_RISING`, `EDGE_FALLING`, `EDGE_BOTH`,
            // `LEVEL_HIGH`, `LEVEL_LOW`.
            const IRQ_TYPES: [u32; 6] = [0, 1, 2, 3, 4, 8];
            if !IRQ_TYPES.contains(flags) {
                warn!(
                    "{MODULE}: a 2-cell specifier whose second cell is {flags:#x} is not <number flags>"
                );
                return None;
            }
            Some(*number)
        }
        (3, [kind, number, _flags, ..]) => match kind {
            // GIC_SPI / GIC_PPI of the binding's own header.
            0 => number.checked_add(32),
            1 => number.checked_add(16),
            // 2 and 3 are the extended SPI and PPI banks of a GICv3, whose
            // bases this builder has no controller for yet.
            _ => {
                warn!("{MODULE}: interrupt type {kind} in a 3-cell specifier is not supported");
                None
            }
        },
        (cells, _) => {
            warn!("{MODULE}: a specifier of {cells} cell(s) has no known shape");
            None
        }
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
            Some(spec) => irq_number(*cells, spec),
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
        // Before anything is mapped: an adopted controller needs no window of
        // its own, and mapping one would be the only side effect of a walk
        // that is otherwise just reading.
        if let Some(irq) = self.adopted(comp) {
            info!(
                "{MODULE}: adopting the running controller for {:?}, whose reg is {:?}",
                node.name,
                parse_reg(node, props)
            );
            return Ok((
                (Device::Irq(irq), interrupts),
                IntcProps {
                    phandle,
                    interrupt_cells,
                },
            ));
        }
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
            // El GENET v5 del BCM2711: la Raspberry Pi 4.
            //
            // Todo lo que el arbol sabe de el --la direccion MDIO del PHY, el
            // modo del RGMII y la MAC de la placa-- lo lee
            // [`genet_from_dt`](crate::net::genet_from_dt), que esta al lado
            // del driver y no aqui porque aqui no hay forma de ponerle un MAC
            // falso debajo para probarlo.
            #[cfg(target_arch = "aarch64")]
            c if c.contains("brcm,bcm2711-genet-v5") => {
                // La ventana del GENET son 64 KiB y los descriptores viven
                // DENTRO de ella: un mapeo mas corto no se nota al escribir
                // los registros, se nota al usar los anillos.
                let base = base_vaddr?;
                Arc::new(unsafe {
                    genet_from_dt(
                        alloc::string::String::from("eth0"),
                        node,
                        base,
                        irq_num as usize,
                    )
                }?)
            }
            // El Cadence GEM del RP1: la Raspberry Pi 5.
            //
            // Al contrario que el GENET, **la ventana no tiene que dar para
            // los descriptores**: los del GEM viven en RAM, no dentro de la
            // ventana de registros. Los 0x4000 que el nodo declara dan para
            // todo lo que el driver toca.
            //
            // Tres cosas que esta rama no puede arreglar y conviene que esten
            // dichas donde se sonda:
            //
            // 1. **El `reg` del nodo es una direccion de bus del PCIe del
            //    BCM2712** (`0xc0_40100000`). `parse_reg` ya la traduce por
            //    los `ranges` de los dos puentes y da la direccion de CPU
            //    (`0x1f_00100000`), pero **que la direccion sea correcta no
            //    levanta el enlace**: Eclipse no tiene driver del controlador
            //    PCIe de Broadcom STB, asi que en una Pi 5 de verdad esa
            //    ventana no contesta hasta que alguien lo escriba.
            // 2. **El RP1 es su propio controlador de interrupciones**
            //    (`#interrupt-cells = <2>`), y Eclipse no tiene driver suyo,
            //    asi que `register_interrupts` no encuentra a quien registrar
            //    la linea.
            // 3. **El nodo puede no estar en el DTB base**: en el arbol de
            //    Raspberry Pi el RP1 se descubre por PCIe y su driver aplica un
            //    overlay en caliente.
            //
            // O sea que esta rama es hoy el camino de un arbol que alguien le
            // pase a mano por la escotilla `dtb=0x…`. Eso no la hace inutil:
            // es lo que hace que el driver tenga llamante, y lo que habra que
            // tocar el dia que el PCIe suba.
            #[cfg(target_arch = "aarch64")]
            c if c.contains("cdns,macb") => {
                let base = base_vaddr?;
                Arc::new(unsafe {
                    macb_from_dt(
                        alloc::string::String::from("eth0"),
                        node,
                        base,
                        irq_num as usize,
                    )
                }?)
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
        // Three cells per specifier, la forma del GIC `<tipo numero flags>`:
        // dos interrupciones, y el numero de cada una es la segunda celda
        // mas los 32 de los SPI.
        let (dev_list, intc_map) = one_controller(
            intc.clone(),
            3,
            InterruptsProp {
                parent: Some(7),
                specs: vec![0, 35, 4, 0, 36, 4],
            },
        );
        assert_eq!(run(&dev_list, &intc_map), Ok(()));
        assert_eq!(seen(&intc).0, vec![35 + 32, 36 + 32]);
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
        // Tres celdas por especificador y siete de lista: la ultima no es un
        // especificador. Cortar mas alla del final colgaba el kernel.
        let (dev_list, intc_map) = one_controller(
            intc.clone(),
            3,
            InterruptsProp {
                parent: Some(7),
                specs: vec![0, 35, 4, 0, 36, 4, 0],
            },
        );
        assert_eq!(run(&dev_list, &intc_map), Ok(()));
        assert_eq!(seen(&intc).0, vec![35 + 32, 36 + 32]);
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
                    crate::uart::Uart16550Mmio::<u8>::new(ram.page)
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
    struct RamMapper {
        page: usize,
        /// Every window the walk asked for, so a test can tell a walk that
        /// only reads from one that touched the bus.
        mapped: Mutex<Vec<(PhysAddr, usize)>>,
    }
    impl RamMapper {
        fn new() -> Self {
            let page: Box<[u64; 512]> = Box::new([0; 512]);
            Self {
                page: Box::leak(page).as_ptr() as usize,
                mapped: Mutex::new(Vec::new()),
            }
        }
    }
    impl IoMapper for RamMapper {
        fn query_or_map(&self, paddr: PhysAddr, size: usize) -> Option<VirtAddr> {
            self.mapped.lock().push((paddr, size));
            Some(self.page)
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
        builder(root, Vec::new()).build()
    }

    fn builder(
        root: Node,
        adopted_intc: Vec<(String, Arc<dyn IrqScheme>)>,
    ) -> DevicetreeDriverBuilder<RamMapper> {
        DevicetreeDriverBuilder {
            dt: Devicetree::from_root(root),
            io_mapper: RamMapper::new(),
            adopted_intc,
        }
    }

    /// A `ns16550a` with no interrupts, which the walk builds a driver for.
    fn a_uart_node() -> Node {
        leaf(
            "serial@2500000",
            vec![
                prop("compatible", strs(&["ns16550a"])),
                prop("reg", cells(&[0x0250_0000, 0x400])),
            ],
        )
    }

    /// The node of a GIC-400 as the `virt` machine and the Raspberry Pi 4
    /// both write it: two `reg` ranges, three cells per specifier, and no
    /// driver of its own on this host.
    fn a_gic_node() -> Node {
        leaf(
            "interrupt-controller@8000000",
            vec![
                prop("compatible", strs(&["arm,gic-400", "arm,cortex-a15-gic"])),
                prop("interrupt-controller", Vec::new()),
                prop("#interrupt-cells", cells(&[3])),
                prop("phandle", cells(&[1])),
                prop(
                    "reg",
                    cells(&[0x0800_0000, 0x1_0000, 0x0801_0000, 0x1_0000]),
                ),
            ],
        )
    }

    /// A UART that asks for SPI 1 of that controller, the way the `virt`
    /// machine's `pl011` does.
    fn a_uart_under_the_gic() -> Node {
        leaf(
            "serial@9000000",
            vec![
                prop("compatible", strs(&["ns16550a"])),
                prop("reg", cells(&[0x0900_0000, 0x1000])),
                prop("interrupt-parent", cells(&[1])),
                prop("interrupts", cells(&[0, 1, 4])),
            ],
        )
    }

    #[test]
    fn the_walk_sends_both_ethernet_macs_to_parse_ethernet() {
        // El defecto que este test clava: el GENET de la Pi 4 entro con su
        // rama DENTRO de `parse_ethernet` y el `match` de fuera seguia
        // nombrando solo al GMAC del D1, asi que con el nodo de la Pi delante
        // nunca se llamaba. No se puede comprobar de punta a punta aqui: la
        // rama del GENET esta detras de `cfg(target_arch = "aarch64")`, asi
        // que en el anfitrion un nodo no sondado y un nodo sondado-y-no-
        // soportado dan los dos `NotSupported`. Lo que si se puede comprobar
        // es el predicado que decide si se llama.
        let gmac = strs(&["allwinner,sunxi-gmac"]);
        assert!(is_ethernet(&StringList::new(&gmac)));
        let genet = strs(&["brcm,bcm2711-genet-v5"]);
        assert!(is_ethernet(&StringList::new(&genet)));
        // Y con la lista entera del nodo de la Pi 4, que nombra el de su
        // version y detras el generico de la familia.
        let pi = strs(&["brcm,bcm2711-genet-v5", "brcm,genet-v5"]);
        assert!(is_ethernet(&StringList::new(&pi)));
        // Y el GEM del RP1 de la Pi 5, con la lista entera de su nodo: el
        // primero es el nombre de la integracion y el segundo el del silicio,
        // y es el **segundo** el que decide, porque es el que dice que este
        // driver sabe manejarlo.
        let rp1 = strs(&["raspberrypi,rp1-gem", "cdns,macb"]);
        assert!(is_ethernet(&StringList::new(&rp1)));
        // Un GEM del mismo IP en otra placa declara el `cdns,macb` y no el
        // `rp1-gem`: tambien tiene que llegar.
        let gem = strs(&["cdns,macb"]);
        assert!(is_ethernet(&StringList::new(&gem)));
    }

    /// Y el nombre de la integracion **a solas** no basta.
    ///
    /// Si el predicado mirara el `raspberrypi,rp1-gem`, un nodo que lo
    /// declarara sin el `cdns,macb` --- otra revision del RP1 con otro MAC
    /// detras --- llegaria a `parse_ethernet` y se programaria como si fuera
    /// un Cadence GEM.
    #[test]
    fn the_integrations_name_on_its_own_is_not_enough_to_claim_the_node() {
        let solo = strs(&["raspberrypi,rp1-gem"]);
        assert!(!is_ethernet(&StringList::new(&solo)));
    }

    #[test]
    fn the_walk_does_not_send_anything_else_to_parse_ethernet() {
        for comp in [
            "brcm,bcm2835-aux-uart",
            "brcm,genet-mdio-v5",
            "raspberrypi,rp1-clocks",
            "raspberrypi,rp1-mbox",
            "snps,dw-apb-uart",
            "virtio,mmio",
        ] {
            let buf = strs(&[comp]);
            assert!(
                !is_ethernet(&StringList::new(&buf)),
                "{} no es un MAC y no debe llegar a parse_ethernet",
                comp
            );
        }
    }

    // ---- adopting a controller that is already running --------------------

    #[test]
    fn a_tree_whose_controller_has_no_driver_here_cannot_be_walked_at_all() {
        // The contrast for the test below, and the position aarch64 is in
        // today: `parse_intc` has no arm for `arm,gic-400` on this host, so
        // the node is turned down, nothing goes into the phandle map, and the
        // UART that names phandle 1 takes the whole walk down with it. A tree
        // of a real board is not partly usable: it is unusable.
        let root = soc(vec![a_gic_node(), a_uart_under_the_gic()]);
        assert_eq!(build(root).err(), Some(DeviceError::InvalidParam));
    }

    #[test]
    fn an_adopted_controller_takes_the_interrupts_of_the_tree() {
        let gic = FakeIntc::new();
        let devs = builder(
            soc(vec![a_gic_node(), a_uart_under_the_gic()]),
            vec![("arm,gic-400".to_string(), gic.clone() as Arc<dyn IrqScheme>)],
        )
        .build()
        .unwrap();
        // The controller comes back in the list as itself, and the UART with
        // it.
        assert_eq!(devs.len(), 2);
        assert!(matches!(devs[0], Device::Irq(_)));
        assert!(matches!(devs[1], Device::Uart(_)));
        // And its SPI 1 was registered as the GIC's own id 33, which is the
        // three-cell reading of `<0 1 4>`.
        assert_eq!(seen(&gic), (vec![33], vec![33]));
    }

    #[test]
    fn adopting_is_by_compatible_and_any_of_the_strings_in_it_counts() {
        // The Pi 4 writes `"arm,gic-400"` first and the `virt` machine of
        // QEMU writes `"arm,cortex-a15-gic"`; a kernel that wants both says
        // both, and a node that names neither is not adopted.
        for named in ["arm,gic-400", "arm,cortex-a15-gic"] {
            let gic = FakeIntc::new();
            let devs = builder(
                soc(vec![a_gic_node(), a_uart_under_the_gic()]),
                vec![(named.to_string(), gic.clone() as Arc<dyn IrqScheme>)],
            )
            .build()
            .unwrap();
            assert_eq!(devs.len(), 2, "{named}");
            assert_eq!(seen(&gic), (vec![33], vec![33]), "{named}");
        }
        let gic = FakeIntc::new();
        let built = builder(
            soc(vec![a_gic_node(), a_uart_under_the_gic()]),
            vec![("arm,gic-600".to_string(), gic.clone() as Arc<dyn IrqScheme>)],
        )
        .build();
        assert_eq!(built.err(), Some(DeviceError::InvalidParam));
        assert_eq!(seen(&gic), (vec![], vec![]));
    }

    #[test]
    fn adopting_maps_no_window_of_its_own() {
        // The controller handed over is already running on a window somebody
        // else mapped. Mapping its `reg` again would be the one side effect of
        // a walk that is otherwise just reading, and on a board whose
        // distributor and CPU interface are two ranges it would be the wrong
        // one anyway: `parse_reg` only ever returns the first.
        let gic = FakeIntc::new();
        let b = builder(
            soc(vec![a_gic_node()]),
            vec![("arm,gic-400".to_string(), gic.clone() as Arc<dyn IrqScheme>)],
        );
        b.build().unwrap();
        assert_eq!(b.io_mapper.mapped.lock().as_slice(), &[]);
    }

    #[test]
    fn a_controller_that_was_not_adopted_still_maps_its_own_window() {
        // The other side of the same claim: the walk does map for a node it
        // builds a driver for, so the assertion above is about adoption and
        // not about the walk having stopped doing anything.
        let b = builder(soc(vec![a_uart_node()]), Vec::new());
        b.build().unwrap();
        assert_eq!(
            b.io_mapper.mapped.lock().as_slice(),
            &[(0x0250_0000, 0x400)]
        );
    }

    #[test]
    fn a_one_cell_specifier_is_the_number_itself() {
        // El PLIC de riscv y el `riscv,cpu-intc`, que es lo unico que llegaba
        // aqui antes. La celda de mas esta para comprobar que no se mira.
        assert_eq!(irq_number(1, &[36]), Some(36));
        assert_eq!(irq_number(1, &[36, 99]), Some(36));
    }

    #[test]
    fn a_three_cell_specifier_is_a_gic_one_and_the_first_cell_is_the_type() {
        // El defecto que esto arregla: con `<0 29 4>` --un SPI, el numero 29,
        // flanco-- tomar la primera celda por el numero registraba el
        // dispositivo en la IRQ 0. El GIC cuenta los SPI desde 32 y los PPI
        // desde 16, que es el desplazamiento que pone Linux.
        assert_eq!(irq_number(3, &[0, 29, 4]), Some(29 + 32));
        assert_eq!(irq_number(3, &[1, 13, 8]), Some(13 + 16));
        // El GENET de la Pi 4 nombra `<0 157 4>` y `<0 158 4>`.
        assert_eq!(irq_number(3, &[0, 157, 4]), Some(189));
        assert_eq!(irq_number(3, &[0, 158, 4]), Some(190));
    }

    /// Dos celdas son `<number flags>`, que es la forma del RP1 de la Pi 5.
    ///
    /// Un controlador asi numera sus propias lineas desde cero, asi que el
    /// numero **no lleva desplazamiento**: sumarle los 32 de un SPI del GIC
    /// registraria el dispositivo en una linea que ese controlador no tiene.
    #[test]
    fn two_cells_are_a_number_and_its_trigger_type() {
        // `interrupts = <RP1_INT_ETH IRQ_TYPE_LEVEL_HIGH>` del nodo
        // `ethernet@100000` de `rp1.dtsi`. `RP1_INT_ETH` es el 6, de
        // `dt-bindings/mfd/rp1.h`.
        assert_eq!(irq_number(2, &[6, 4]), Some(6));
        // Los seis `IRQ_TYPE_*` de `dt-bindings/interrupt-controller/irq.h`,
        // que son todos los que hay.
        for flags in [0, 1, 2, 3, 4, 8] {
            assert_eq!(irq_number(2, &[29, flags]), Some(29), "flags {flags}");
        }
        // Y la linea cero de un controlador asi es una linea: no se puede
        // tratar como «no hay numero», que es lo que pasaba con el GIC.
        assert_eq!(irq_number(2, &[0, 4]), Some(0));
    }

    #[test]
    fn a_specifier_shape_nobody_here_has_read_is_refused_not_guessed() {
        // Los bancos extendidos de un GICv3, para los que no hay controlador.
        assert_eq!(irq_number(3, &[2, 0, 4]), None);
        assert_eq!(irq_number(3, &[3, 0, 4]), None);
        // Un numero de celdas sin forma conocida. Antes se cogia la primera
        // celda igualmente, que es adivinar.
        assert_eq!(irq_number(4, &[0, 0, 7, 4]), None);
        // Y dos celdas cuya segunda no es ningun `IRQ_TYPE_*`: no es
        // `<number flags>`, asi que su primera celda no es un numero.
        assert_eq!(irq_number(2, &[7, 5]), None);
        assert_eq!(irq_number(2, &[7, 0x1000]), None);
        assert_eq!(irq_number(2, &[7]), None);
        // Y un especificador mas corto de lo que dice su cuenta de celdas.
        assert_eq!(irq_number(3, &[0, 29]), None);
        assert_eq!(irq_number(1, &[]), None);
    }

    #[test]
    fn a_number_at_the_top_of_the_range_does_not_wrap_round() {
        // `checked_add`: un numero absurdo en el arbol no debe dar una IRQ
        // pequeña y valida por desbordamiento.
        assert_eq!(irq_number(3, &[0, u32::MAX, 4]), None);
        assert_eq!(irq_number(3, &[1, u32::MAX - 10, 4]), None);
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
