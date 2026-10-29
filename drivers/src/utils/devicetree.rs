//! Package of [`device_tree`].

use crate::{DeviceError, DeviceResult, PhysAddr, VirtAddr};
use alloc::vec::Vec;
use core::ops::Range;
use device_tree::{DeviceTree as DeviceTreeInner, PropError};

pub use device_tree::{util::StringList, Node};

/// A unified representation of the `interrupts` and `interrupts-extended`
/// properties for any interrupt generating device.
///
/// The two spell the same thing differently, and the difference matters:
/// `interrupts-extended` names a parent in front of every specifier, while
/// plain `interrupts` is a list of specifiers that **all** belong to the one
/// parent in `interrupt-parent`. Flattening the second into the first by
/// putting the phandle in once only works for a node with a single interrupt;
/// with two, the second specifier gets read as a phandle. Hence the tag: how
/// long a specifier is only becomes known later, in the builder, once the
/// controller that phandle points at has been parsed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InterruptsProp {
    /// The one parent every specifier belongs to, for a node that used plain
    /// `interrupts`. `None` when the node used `interrupts-extended`, where
    /// each specifier carries its own parent.
    pub parent: Option<u32>,
    /// `[{phandle, args..}*]` when `parent` is `None`, `[{args..}*]` when it
    /// is `Some`.
    pub specs: Vec<u32>,
}

impl InterruptsProp {
    /// Returns `true` when the node asks for no interrupts at all.
    pub fn is_empty(&self) -> bool {
        self.specs.is_empty()
    }

    /// The first interrupt number the node asks for, if it asks for any.
    ///
    /// The number is the first argument of the first specifier, which sits
    /// one cell further along when the specifier names its own parent.
    pub fn first_irq(&self) -> Option<u32> {
        let args_at = if self.parent.is_some() { 0 } else { 1 };
        self.specs.get(args_at).copied()
    }
}

/// A wrapper structure of `device_tree::DeviceTree`.
pub struct Devicetree(DeviceTreeInner);

/// El codigo de espacio de una direccion del binding de PCI.
///
/// La primera celda de una direccion de tres es un mapa de bits, no parte de
/// la direccion, y sus bits 24 y 25 dicen **en que espacio** esta: 0 es el de
/// configuracion, 1 el de entrada/salida, 2 la memoria de 32 bits y 3 la de
/// 64 bits. Los demas bits --- el 30 de prefetchable, el 31 de relocatable ---
/// no entran en la comparacion: dos ventanas del mismo espacio pueden
/// diferir en ellos, y un bus de PCI tiene **una direccion cero en cada
/// espacio**, asi que casar solo por direccion cogeria la ventana de otro.
fn pci_space(flags: u32) -> u32 {
    (flags >> 24) & 0b11
}

/// Como se leen las direcciones de un bus.
///
/// Es lo que decide cuantas celdas de una direccion son direccion. Un bus
/// corriente las usa todas; uno de PCI gasta la primera en el mapa de bits de
/// [`pci_space`], asi que de tres celdas solo dos son el numero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddrFormat {
    /// Todas las celdas son la direccion.
    Plain,
    /// La primera celda son banderas y el resto la direccion. Es lo que
    /// declara un nodo con `device_type = "pci"`.
    Pci,
}

impl Default for AddrFormat {
    /// Un bus corriente, que es lo que es el nodo raiz.
    fn default() -> Self {
        Self::Plain
    }
}

impl AddrFormat {
    /// El formato que declara un nodo.
    pub fn of(node: &Node) -> Self {
        match node.prop_str("device_type") {
            Ok("pci") => Self::Pci,
            _ => Self::Plain,
        }
    }

    /// Parte una direccion en su espacio y su numero.
    ///
    /// Devuelve `None` cuando no hay celdas para lo que el formato pide: en un
    /// bus de PCI hacen falta al menos dos, una de banderas y una de numero.
    fn split(self, cells: &[u32], addr_cells: u32) -> Option<(u32, u64)> {
        let n = addr_cells as usize;
        if cells.len() < n {
            return None;
        }
        match self {
            Self::Plain => Some((0, from_cells(cells, addr_cells).ok()?)),
            Self::Pci if n >= 2 => {
                let espacio = pci_space(cells[0]);
                let numero = from_cells(&cells[1..n], addr_cells - 1).ok()?;
                Some((espacio, numero))
            }
            Self::Pci => None,
        }
    }
}

/// Una ventana de `ranges` ya reducida a direcciones de **CPU**.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CpuWindow {
    /// El espacio de este bus que la ventana cubre (siempre 0 fuera de PCI).
    space: u32,
    /// Donde empieza, en direcciones de este bus.
    base: u64,
    /// Cuanto cubre.
    size: u64,
    /// Y a que direccion de CPU corresponde su primer byte.
    cpu: u64,
}

/// La traduccion de las direcciones de un bus a direcciones de CPU.
///
/// El `reg` de un nodo esta en las direcciones **de su bus**, no en las de la
/// CPU, y los dos coinciden solo mientras todos los nodos de arriba sean buses
/// que no traducen. En la Raspberry Pi 5 no coinciden: el GEM del RP1 declara
/// `reg = <0xc0 0x40100000 ...>`, que pasa por el `ranges` del nodo `rp1`
/// --- hacia el espacio de memoria de 32 bits del PCIe, en `0x0_00100000`
/// --- y por el del puente `pcie@120000` --- hacia la direccion de CPU
/// `0x1f_00100000`. Mapear el `0xc0_40100000` a secas es mapear otra cosa.
///
/// Las ventanas se guardan **ya reducidas**: la lista de un bus lleva sus
/// propias direcciones de origen y direcciones de CPU de destino, porque al
/// construirla se traduce el otro extremo con la traduccion del bus de arriba.
/// Asi traducir es una sola busqueda y no un paseo por los antepasados.
///
/// **Una lista vacia es la identidad**, y es lo que tiene el nodo raiz.
/// Tambien es lo que se hereda de un nodo que **no trae `ranges`**: segun la
/// especificacion eso quiere decir que el bus no traduce y que sus
/// direcciones no son accesibles desde la CPU, pero los arboles con los que
/// Eclipse arranca hoy --- la `virt` de QEMU, la Pi 4 --- cuelgan sus
/// dispositivos de nodos `simple-bus` sin `ranges` y se mapean bien leyendo
/// el `reg` tal cual. Tratar eso como intraducible dejaria esas placas sin
/// consola, asi que la ausencia **no cambia nada**: es una divergencia a
/// proposito, y aqui queda dicha.
#[derive(Clone, Debug, Default)]
pub struct BusTranslation {
    windows: Vec<CpuWindow>,
}

impl BusTranslation {
    /// Si este bus usa las direcciones de la CPU tal cual.
    pub fn is_identity(&self) -> bool {
        self.windows.is_empty()
    }

    /// La direccion de CPU de `len` bytes en `addr` del espacio `space`.
    ///
    /// `None` cuando ninguna ventana la cubre **entera**: una region que
    /// empieza dentro de una ventana y se sale por arriba no esta donde la
    /// ventana dice, y traducir solo su primer byte daria un mapeo que
    /// funciona hasta la primera pagina de mas.
    pub fn translate(&self, space: u32, addr: u64, len: u64) -> Option<u64> {
        if self.is_identity() {
            return Some(addr);
        }
        let fin = addr.checked_add(len)?;
        for w in &self.windows {
            if w.space != space || addr < w.base {
                continue;
            }
            let tope = w.base.checked_add(w.size)?;
            if fin <= tope {
                return Some(w.cpu + (addr - w.base));
            }
        }
        None
    }

    /// La traduccion que heredan los hijos de `node`, cuyo bus es este.
    ///
    /// `node_fmt`/`node_addr_cells`/`node_size_cells` son los del propio
    /// `node`, que es el bus de los hijos; `parent_fmt`/`parent_addr_cells`
    /// los del bus en el que **este** objeto traduce.
    pub fn compose_child(
        &self,
        node: &Node,
        node_fmt: AddrFormat,
        node_addr_cells: u32,
        node_size_cells: u32,
        parent_fmt: AddrFormat,
        parent_addr_cells: u32,
    ) -> Self {
        let Some(cells) = node.prop_raw("ranges").map(|raw| cells_of(raw.as_slice())) else {
            // Sin `ranges` no se cambia nada; ver la nota del tipo.
            return self.clone();
        };
        if cells.is_empty() {
            // `ranges;` a secas es la identidad en este nivel: las
            // direcciones del hijo son las del padre, asi que la traduccion
            // es la misma.
            return self.clone();
        }
        let paso = (node_addr_cells + parent_addr_cells + node_size_cells) as usize;
        if paso == 0 || !cells.len().is_multiple_of(paso) {
            warn!(
                "device-tree: el `ranges` de {:?} mide {} celdas, que no son multiplo de {paso}",
                node.name,
                cells.len()
            );
            return self.clone();
        }
        let mut windows = Vec::new();
        for triple in cells.chunks(paso) {
            let (hijo_fin, padre_fin) = (
                node_addr_cells as usize,
                (node_addr_cells + parent_addr_cells) as usize,
            );
            let Some((space, base)) = node_fmt.split(&triple[..hijo_fin], node_addr_cells) else {
                warn!("device-tree: {:?}: una direccion de hijo del `ranges` no se puede leer con {node_addr_cells} celdas", node.name);
                continue;
            };
            let Some((padre_space, padre_addr)) =
                parent_fmt.split(&triple[hijo_fin..padre_fin], parent_addr_cells)
            else {
                warn!("device-tree: {:?}: una direccion de padre del `ranges` no se puede leer con {parent_addr_cells} celdas", node.name);
                continue;
            };
            let Ok(size) = from_cells(&triple[padre_fin..], node_size_cells) else {
                warn!(
                    "device-tree: {:?}: un tamaño del `ranges` no cabe",
                    node.name
                );
                continue;
            };
            // Y el otro extremo, por la traduccion de arriba: una ventana que
            // el bus de arriba no sabe traducir **no se guarda a medias**,
            // porque guardarla con su direccion de padre como si fuera de CPU
            // es justo el fallo que esto viene a arreglar.
            let Some(cpu) = self.translate(padre_space, padre_addr, size) else {
                warn!(
                    "device-tree: {:?}: la ventana {padre_addr:#x}+{size:#x} del espacio {padre_space} no la traduce el bus de arriba",
                    node.name
                );
                continue;
            };
            windows.push(CpuWindow {
                space,
                base,
                size,
                cpu,
            });
        }
        if windows.is_empty() {
            // Ni una ventana aprovechable. Dejar la identidad seria decir que
            // las direcciones de este bus son de CPU, que es precisamente lo
            // que el `ranges` dice que no. Una lista con una ventana
            // imposible hace que `translate` devuelva `None`, que es la
            // respuesta honrada.
            windows.push(CpuWindow {
                space: u32::MAX,
                base: 0,
                size: 0,
                cpu: 0,
            });
        }
        Self { windows }
    }
}

/// Parte un trozo de propiedad en celdas de 32 bits big endian.
fn cells_of(raw: &[u8]) -> Vec<u32> {
    raw.as_chunks::<4>()
        .0
        .iter()
        .map(|c| u32::from_be_bytes(*c))
        .collect()
}

/// Some properties inherited from ancestor nodes.
///
/// About the notion: cell, see <https://elinux.org/Device_Tree_Usage#How_Addressing_Works>.
#[derive(Clone, Debug, Default)]
pub struct InheritProps {
    /// The `#address-cells` property of its parent node.
    pub parent_address_cells: u32,
    /// The `#size-cells` property of its parent node.
    pub parent_size_cells: u32,
    /// Como se leen las direcciones del bus del padre, que es el bus en el
    /// que esta el `reg` de este nodo.
    pub parent_addr_format: AddrFormat,
    /// Lo que hay que atravesar para convertir una direccion de ese bus en
    /// una direccion de CPU. Ver [`BusTranslation`].
    pub bus: BusTranslation,
    /// The `interrupt-parent` property of the node. If don't have, inherit from
    /// its parent node.
    pub interrupt_parent: u32,
}

/// The magic number at the start of every flattened device tree.
pub const FDT_MAGIC: u32 = 0xd00d_feed;

/// The size of the v17 FDT header, which is the whole of what
/// [`check_fdt_header`] needs to look at.
pub const FDT_HEADER_SIZE: usize = 40;

/// The largest blob this kernel will try to walk.
///
/// The `virt` machine of QEMU hands over a few KiB and the firmware of a
/// Raspberry Pi 4 about 50 KiB, so this is wide by two orders of magnitude.
/// It is here because `totalsize` decides how much memory gets read, and a
/// number read out of the wrong place is as likely to be 3 GiB as 3 KiB.
pub const FDT_MAX_SIZE: usize = 4 << 20;

/// The last structure version whose layout this reader knows.
const FDT_SUPPORTED_VERSION: u32 = 17;

/// Checks the header of a flattened device tree, and returns its `totalsize`.
///
/// **This is what makes an address from outside safe to walk.** The loader of
/// `device_tree` reads `totalsize` out of the second word and builds a slice
/// that long *before* it looks at the magic number, so an address that is not
/// a blob at all --- a bootloader field that was never filled in, a `dtb=`
/// written wrong on the command line --- turns into a slice of up to 4 GiB
/// over whatever happens to follow. Reading the 40 bytes of the header first
/// and refusing anything that does not add up keeps that to one bounded read.
///
/// What it cannot do is make the address itself safe: the header has to be
/// read to be checked, so a caller still owes us an address that is mapped.
/// Everything past the header is checked here instead of trusted.
pub fn check_fdt_header(header: &[u8]) -> DeviceResult<usize> {
    let word = |at: usize| -> DeviceResult<usize> {
        let bytes: &[u8] = header.get(at..at + 4).ok_or(DeviceError::InvalidParam)?;
        let bytes: [u8; 4] =
            core::convert::TryInto::try_into(bytes).map_err(|_| DeviceError::InvalidParam)?;
        Ok(u32::from_be_bytes(bytes) as usize)
    };

    if word(0)? != FDT_MAGIC as usize {
        warn!("device-tree: no FDT magic, got {:#x}", word(0)?);
        return Err(DeviceError::InvalidParam);
    }
    let totalsize = word(4)?;
    if !(FDT_HEADER_SIZE..=FDT_MAX_SIZE).contains(&totalsize) {
        warn!("device-tree: totalsize {totalsize:#x} is not a size a blob can have");
        return Err(DeviceError::InvalidParam);
    }
    // `version` is what the blob is, `last_comp_version` the oldest reader it
    // still works with. A newer blob is fine as long as it says so; one whose
    // floor is above us is not, and reading it would mean guessing a layout.
    let last_comp_version = word(24)?;
    if last_comp_version > FDT_SUPPORTED_VERSION as usize {
        warn!("device-tree: the blob needs a reader of version {last_comp_version} or newer");
        return Err(DeviceError::InvalidParam);
    }
    // Each block has to lie inside the blob. The additions are checked
    // because an offset near `usize::MAX` plus a size wraps round into a
    // range that looks perfectly reasonable.
    for (name, off_at, size_at, align) in [
        ("struct", 8, Some(36), 4),
        ("strings", 12, Some(32), 1),
        // The reservation block has no size of its own: it ends at its
        // terminator, somewhere inside the blob.
        ("memory reservations", 16, None, 8),
    ] {
        let off = word(off_at)?;
        if off % align != 0 {
            warn!("device-tree: the {name} block at {off:#x} is not aligned to {align}");
            return Err(DeviceError::InvalidParam);
        }
        let size = match size_at {
            Some(at) => word(at)?,
            None => 0,
        };
        match off.checked_add(size) {
            Some(end) if end <= totalsize => {}
            _ => {
                warn!("device-tree: the {name} block ({off:#x}+{size:#x}) is not inside the blob");
                return Err(DeviceError::InvalidParam);
            }
        }
    }
    Ok(totalsize)
}

impl Devicetree {
    /// Load the device tree blob from the given virtual address.
    ///
    /// The address has to be mapped and readable for at least
    /// [`FDT_HEADER_SIZE`] bytes; everything beyond that is decided by the
    /// header, after [`check_fdt_header`] has agreed with it.
    pub fn from(dtb_base_vaddr: VirtAddr) -> DeviceResult<Self> {
        info!("Loading device tree blob from {:#x}", dtb_base_vaddr);
        // SAFETY: the caller owes us a mapped address, and this is the one
        // read made on its word alone. Its result bounds every other.
        let header =
            unsafe { core::slice::from_raw_parts(dtb_base_vaddr as *const u8, FDT_HEADER_SIZE) };
        let totalsize = check_fdt_header(header).inspect_err(|_| {
            warn!("device-tree: refusing the blob @ {dtb_base_vaddr:#x}");
        })?;
        // SAFETY: `totalsize` came out of a header that agreed with itself,
        // and is at most `FDT_MAX_SIZE`.
        let blob = unsafe { core::slice::from_raw_parts(dtb_base_vaddr as *const u8, totalsize) };
        match DeviceTreeInner::load(blob) {
            Ok(dt) => Ok(Self(dt)),
            Err(err) => {
                warn!(
                    "device-tree: failed to load DTB @ {:#x}: {:?}",
                    dtb_base_vaddr, err
                );
                Err(DeviceError::InvalidParam)
            }
        }
    }

    fn walk_inner<F>(&self, node: &Node, props: InheritProps, device_node_op: &mut F)
    where
        F: FnMut(&Node, &StringList, &InheritProps),
    {
        let mut props = props;
        if let Ok(num) = node.prop_u32("interrupt-parent") {
            props.interrupt_parent = num;
        }
        if let Ok(comp) = node.prop_str_list("compatible") {
            device_node_op(node, &comp, &props);
        }

        // El bus de los hijos es ESTE nodo, asi que lo que se recoge aqui son
        // sus celdas, su formato y su `ranges`. El orden importa: la
        // traduccion de los hijos se compone con la que este nodo acaba de
        // usar para su propio `reg`, que es la de su padre, asi que se calcula
        // **antes** de sobreescribir las celdas del padre.
        let node_fmt = AddrFormat::of(node);
        let node_address_cells = node.prop_u32("#address-cells").unwrap_or(0);
        let node_size_cells = node.prop_u32("#size-cells").unwrap_or(0);
        props.bus = props.bus.compose_child(
            node,
            node_fmt,
            node_address_cells,
            node_size_cells,
            props.parent_addr_format,
            props.parent_address_cells,
        );
        props.parent_address_cells = node_address_cells;
        props.parent_size_cells = node_size_cells;
        props.parent_addr_format = node_fmt;

        // DFS
        for child in node.children.iter() {
            self.walk_inner(child, props.clone(), device_node_op);
        }
    }

    /// Builds a tree straight from an already parsed root node.
    ///
    /// Only for tests: every other caller comes in through [`Devicetree::from`],
    /// which needs a real blob at a real address.
    #[cfg(test)]
    pub fn from_root(root: Node) -> Self {
        Self(DeviceTreeInner {
            version: 17,
            boot_cpuid_phys: 0,
            reserved: Vec::new(),
            root,
        })
    }

    /// El nodo raiz, para poder colgarle algo. Solo para los tests.
    #[cfg(test)]
    pub fn root_mut(&mut self) -> &mut Node {
        &mut self.0.root
    }

    /// Traverse the tree from root by DFS, collect necessary properties, and
    /// apply the `device_node_op` to each node.
    pub fn walk<F>(&self, device_node_op: &mut F)
    where
        F: FnMut(&Node, &StringList, &InheritProps),
    {
        self.walk_inner(&self.0.root, InheritProps::default(), device_node_op)
    }

    /// Returns the `bootargs` property in the `/chosen` node, as the kernel
    /// command line.
    pub fn bootargs(&self) -> Option<&str> {
        self.0.find("/chosen")?.prop_str("bootargs").ok()
    }

    /// Returns the `timebase-frequency` property in the `/cpus` node, as timer
    pub fn timebase_frequency(&self) -> Option<u32> {
        self.0.find("/cpus")?.prop_u32("timebase-frequency").ok()
    }

    /// Returns the `linux,initrd-start` and `linux,initrd-end` properties in
    /// the `/chosen` node, as the init RAM disk address region.
    pub fn initrd_region(&self) -> Option<Range<PhysAddr>> {
        let chosen = self.0.find("/chosen")?;
        let start = chosen.prop_cells("linux,initrd-start").ok()?;
        let end = chosen.prop_cells("linux,initrd-end").ok()?;
        let start = from_cells(&start, start.len() as u32).ok()? as PhysAddr;
        let end = from_cells(&end, end.len() as u32).ok()? as PhysAddr;
        // `init_ram_disk` turns this range into a slice at `start`, so a
        // backwards one is a slice of a length nobody chose.
        if end < start {
            warn!("device-tree: initrd ends before it starts: {start:#x}..{end:#x}");
            return None;
        }
        Some(start..end)
    }

    /// Returns the physical memory regions specified in the `/memory` nodes.
    pub fn memory_regions(&self) -> DeviceResult<Vec<Range<PhysAddr>>> {
        let props = InheritProps {
            parent_address_cells: self.0.root.prop_u32("#address-cells").unwrap_or(0),
            parent_size_cells: self.0.root.prop_u32("#size-cells").unwrap_or(0),
            ..Default::default()
        };

        let mut regions = Vec::new();
        for node in &self.0.root.children {
            if node.name.starts_with("memory@")
                || node.prop_str("device_type").unwrap_or_default() == "memory"
            {
                let (addr, size) = parse_reg(node, &props)?;
                // These two come straight out of the blob and land in the frame
                // allocator. Adding them up used to panic the kernel in a debug
                // build and wrap around in a release one, which is worse: the
                // range comes out backwards and the allocator is handed memory
                // that is not there.
                match (addr as usize).checked_add(size as usize) {
                    Some(end) => regions.push(addr as usize..end),
                    None => warn!(
                        "device-tree: memory node {:?} runs off the end of the address space: {:#x} + {:#x}",
                        node.name, addr, size
                    ),
                }
            }
        }
        Ok(regions)
    }
}

/// Combine `cell_num` of 32-bit integers from `cells` into a 64-bit integer.
fn from_cells(cells: &[u32], cell_num: u32) -> DeviceResult<u64> {
    if cell_num as usize > cells.len() {
        return Err(DeviceError::InvalidParam);
    }
    // More than two cells does not fit in the u64 every caller wants, and
    // `value << 32` quietly drops whatever ran off the top: the address that
    // came back was the low half of the one in the blob, which is an address
    // all the same and gets mapped. Say so instead.
    if cell_num > 2 && cells[..cell_num as usize - 2].iter().any(|&c| c != 0) {
        warn!("device-tree: {cell_num} cells do not fit in 64 bits: {cells:#x?}");
        return Err(DeviceError::InvalidParam);
    }
    let mut value = 0;
    for &c in &cells[..cell_num as usize] {
        value = value << 32 | c as u64;
    }
    Ok(value)
}

/// Parse the `reg` property, about `reg`: <https://elinux.org/Device_Tree_Usage#How_Addressing_Works>.
///
/// Lo que devuelve es una direccion **de CPU**, no la del `reg`. Las dos
/// coinciden mientras ningun nodo de arriba traduzca --- que es el caso de la
/// `virt` de QEMU y de la Pi 4 --- y no coinciden en la Pi 5, donde el
/// dispositivo cuelga de un puente de PCIe. Todo el que use esto se lo pasa a
/// un mapeador, asi que devolver la direccion del bus seria mapear otra cosa;
/// ver [`BusTranslation`].
///
/// `Err` cuando el bus declara una traduccion y ninguna de sus ventanas cubre
/// la region: no hay direccion de CPU que dar, y la del bus **no lo es**.
pub fn parse_reg(node: &Node, props: &InheritProps) -> DeviceResult<(u64, u64)> {
    let cells = node.prop_cells("reg")?;
    let (space, addr) = props
        .parent_addr_format
        .split(&cells, props.parent_address_cells)
        .ok_or(DeviceError::InvalidParam)?;
    let size = from_cells(
        &cells[props.parent_address_cells as usize..],
        props.parent_size_cells,
    )?;
    let cpu = props.bus.translate(space, addr, size).ok_or_else(|| {
        warn!(
            "device-tree: el bus de {:?} no traduce {addr:#x}+{size:#x} del espacio {space}",
            node.name
        );
        DeviceError::InvalidParam
    })?;
    Ok((cpu, size))
}

/// Returns a `Vec<u32>` according to the `interrupts` or `interrupts-extended`
/// property, the first element is the interrupt parent.
pub fn parse_interrupts(node: &Node, props: &InheritProps) -> DeviceResult<InterruptsProp> {
    if node.has_prop("interrupts-extended") {
        Ok(InterruptsProp {
            parent: None,
            specs: node.prop_cells("interrupts-extended")?,
        })
    } else if node.has_prop("interrupts") && props.interrupt_parent > 0 {
        Ok(InterruptsProp {
            parent: Some(props.interrupt_parent),
            specs: node.prop_cells("interrupts")?,
        })
    } else {
        Ok(InterruptsProp::default())
    }
}

impl From<PropError> for DeviceError {
    fn from(_err: PropError) -> Self {
        Self::InvalidParam
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{
        string::{String, ToString},
        vec,
    };

    /// The ten words of a v17 FDT header that agrees with itself.
    ///
    /// The numbers are those of the `virt` machine's own blob, scaled down:
    /// a header, then the reservation block, the struct block and the string
    /// table, in the order `dtc` emits them.
    fn a_header() -> Vec<u32> {
        vec![
            FDT_MAGIC, // 0: magic
            0x200,     // 4: totalsize
            0x48,      // 8: off_dt_struct
            0x180,     // 12: off_dt_strings
            0x28,      // 16: off_mem_rsvmap
            17,        // 20: version
            16,        // 24: last_comp_version
            0,         // 28: boot_cpuid_phys
            0x40,      // 32: size_dt_strings
            0x138,     // 36: size_dt_struct
        ]
    }

    fn bytes_of(words: &[u32]) -> Vec<u8> {
        cells(words)
    }

    /// `a_header` with one word replaced, which is how every refusal below is
    /// built: one field wrong and nothing else.
    fn header_with(at: usize, value: u32) -> Vec<u8> {
        let mut words = a_header();
        words[at / 4] = value;
        bytes_of(&words)
    }

    #[test]
    fn a_header_that_agrees_with_itself_gives_its_totalsize() {
        assert_eq!(check_fdt_header(&bytes_of(&a_header())), Ok(0x200));
        // And trailing bytes are ignored: the caller hands over a whole blob,
        // not just the header.
        let mut blob = bytes_of(&a_header());
        blob.extend_from_slice(&[0xAA; 0x100]);
        assert_eq!(check_fdt_header(&blob), Ok(0x200));
    }

    #[test]
    fn an_address_that_is_not_a_blob_is_refused_whatever_its_size_word_says() {
        // This is the whole point of the check. `load_from_raw_pointer` reads
        // `totalsize` first and builds a slice that long, so an address that
        // is not a blob gives a slice of up to 4 GiB over whatever follows.
        // The order *inside* this function is not what saves us --- reading a
        // word is harmless --- it is that `Devicetree::from` does not touch
        // `totalsize` until this has agreed with the magic.
        let not_a_blob = header_with(0, 0x1234_5678);
        assert_eq!(
            check_fdt_header(&not_a_blob),
            Err(DeviceError::InvalidParam)
        );
        // And an address of noise is refused on the magic, before the 3 GiB
        // in its second word is ever used as a length.
        let mut words = a_header();
        words[0] = 0x1234_5678;
        words[1] = 0xC000_0000;
        assert_eq!(
            check_fdt_header(&bytes_of(&words)),
            Err(DeviceError::InvalidParam)
        );
    }

    #[test]
    fn a_size_a_blob_cannot_have_is_refused() {
        // Zero, which is what an unfilled bootloader field reads as.
        assert_eq!(
            check_fdt_header(&header_with(4, 0)),
            Err(DeviceError::InvalidParam)
        );
        // Smaller than its own header.
        assert_eq!(
            check_fdt_header(&header_with(4, FDT_HEADER_SIZE as u32 - 1)),
            Err(DeviceError::InvalidParam)
        );
        // And the 3 GiB that a word of noise is as likely to be as 3 KiB.
        assert_eq!(
            check_fdt_header(&header_with(4, 0xC000_0000)),
            Err(DeviceError::InvalidParam)
        );
        // Exactly the ceiling still passes: it is a limit, not a margin.
        let mut words = a_header();
        words[1] = FDT_MAX_SIZE as u32;
        assert_eq!(check_fdt_header(&bytes_of(&words)), Ok(FDT_MAX_SIZE));
    }

    #[test]
    fn a_blob_that_needs_a_newer_reader_is_refused_and_a_newer_blob_is_not() {
        // `last_comp_version` is the oldest reader the blob still works with.
        assert_eq!(
            check_fdt_header(&header_with(24, 18)),
            Err(DeviceError::InvalidParam)
        );
        // 17 is the layout this reader knows, so a blob whose floor is 17 is
        // fine --- and so is one that calls itself version 99 as long as its
        // floor is not above us, which is what the spec promises.
        assert_eq!(check_fdt_header(&header_with(24, 17)), Ok(0x200));
        assert_eq!(check_fdt_header(&header_with(20, 99)), Ok(0x200));
    }

    #[test]
    fn a_block_that_falls_outside_the_blob_is_refused() {
        // The struct block starting past the end.
        assert_eq!(
            check_fdt_header(&header_with(8, 0x400)),
            Err(DeviceError::InvalidParam)
        );
        // The struct block starting inside but ending past it.
        assert_eq!(
            check_fdt_header(&header_with(36, 0x1F0)),
            Err(DeviceError::InvalidParam)
        );
        // And the string table, which has its own pair of words.
        assert_eq!(
            check_fdt_header(&header_with(32, 0x1000)),
            Err(DeviceError::InvalidParam)
        );
        assert_eq!(
            check_fdt_header(&header_with(12, 0x400)),
            Err(DeviceError::InvalidParam)
        );
        // The reservation block has no size, so only its offset is checked.
        assert_eq!(
            check_fdt_header(&header_with(16, 0x400)),
            Err(DeviceError::InvalidParam)
        );
    }

    #[test]
    fn a_block_near_the_top_of_the_range_is_refused() {
        // On a 32-bit target `off + size` here wraps into a range that looks
        // perfectly reasonable, which is why the addition is checked; on this
        // 64-bit host it cannot, because both words came from a `u32`. What
        // the test can prove is that such an offset is refused either way.
        assert_eq!(
            check_fdt_header(&header_with(8, 0xFFFF_FFFC)),
            Err(DeviceError::InvalidParam)
        );
    }

    #[test]
    fn a_header_whose_blocks_are_all_empty_still_has_to_be_big_enough() {
        // The floor on `totalsize` is the only thing that catches this one: a
        // header of zeros with the magic patched in passes every containment
        // check, because nothing is anywhere and nothing has a size.
        let mut words = vec![0u32; 10];
        words[0] = FDT_MAGIC;
        words[1] = 4;
        assert_eq!(
            check_fdt_header(&bytes_of(&words)),
            Err(DeviceError::InvalidParam)
        );
        // With a size a blob can have, the same header is accepted: whether
        // its blocks say anything is for the parser to find out.
        words[1] = FDT_HEADER_SIZE as u32;
        assert_eq!(check_fdt_header(&bytes_of(&words)), Ok(FDT_HEADER_SIZE));
    }

    #[test]
    fn the_reservation_block_may_sit_at_the_very_end_because_it_has_no_size() {
        // It ends at its terminator, so only its offset is checked. Giving it
        // the struct block's size by mistake would refuse this header.
        let mut words = a_header();
        words[4] = 0x1F8;
        assert_eq!(check_fdt_header(&bytes_of(&words)), Ok(0x200));
        // One word further and it is outside the blob.
        words[4] = 0x200;
        assert_eq!(check_fdt_header(&bytes_of(&words)), Ok(0x200));
        words[4] = 0x208;
        assert_eq!(
            check_fdt_header(&bytes_of(&words)),
            Err(DeviceError::InvalidParam)
        );
    }

    #[test]
    fn a_misaligned_block_is_refused() {
        // Every word of the struct block is read as a big-endian `u32`, and
        // the reservation entries as pairs of `u64`.
        assert_eq!(
            check_fdt_header(&header_with(8, 0x49)),
            Err(DeviceError::InvalidParam)
        );
        assert_eq!(
            check_fdt_header(&header_with(16, 0x24)),
            Err(DeviceError::InvalidParam)
        );
        // The string table is a run of bytes, so it has no alignment to meet.
        assert_eq!(check_fdt_header(&header_with(12, 0x181)), Ok(0x200));
    }

    #[test]
    fn a_buffer_too_short_to_hold_a_header_is_refused_not_read_past() {
        let whole = bytes_of(&a_header());
        for len in 0..FDT_HEADER_SIZE {
            assert_eq!(
                check_fdt_header(&whole[..len]),
                Err(DeviceError::InvalidParam),
                "{len} bytes"
            );
        }
    }

    /// A `<u32 u32 ...>` property value, in the blob's big-endian order.
    fn cells(v: &[u32]) -> Vec<u8> {
        let mut out = Vec::new();
        for c in v {
            out.extend_from_slice(&c.to_be_bytes());
        }
        out
    }

    /// A `"a", "b"` property value: NUL-terminated strings back to back.
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

    fn node(name: &str, props: Vec<(String, Vec<u8>)>, children: Vec<Node>) -> Node {
        Node {
            name: name.to_string(),
            props,
            children,
        }
    }

    fn leaf(name: &str, props: Vec<(String, Vec<u8>)>) -> Node {
        node(name, props, Vec::new())
    }

    /// A minimal flattened device tree, written by hand.
    ///
    /// `DeviceTree::store` of the vendored `device_tree` cannot be used for
    /// this: the blob it emits comes back out of its own `load` as
    /// `UnexpectedEndOfInput`, padded or not, so it is no use as a reference
    /// emitter. Writing the bytes here instead is also the point --- a
    /// hand-made *header* only proves the check agrees with itself, while a
    /// whole blob laid out the way the spec says proves the ten words are
    /// where this reader looks for them.
    struct Fdt {
        structs: Vec<u8>,
        strings: Vec<u8>,
    }

    impl Fdt {
        const BEGIN_NODE: u32 = 1;
        const END_NODE: u32 = 2;
        const PROP: u32 = 3;
        const END: u32 = 9;

        fn new() -> Self {
            Self {
                structs: Vec::new(),
                strings: Vec::new(),
            }
        }

        fn token(&mut self, t: u32) {
            self.structs.extend_from_slice(&t.to_be_bytes());
        }

        /// Appends to the string table and returns the offset into it, reusing
        /// a name that is already there, the way `dtc` does.
        fn name_off(&mut self, name: &str) -> u32 {
            let wanted = strs(&[name]);
            if let Some(at) = self
                .strings
                .windows(wanted.len())
                .position(|w| w == wanted.as_slice())
            {
                return at as u32;
            }
            let at = self.strings.len() as u32;
            self.strings.extend_from_slice(&wanted);
            at
        }

        fn pad_structs(&mut self) {
            while !self.structs.len().is_multiple_of(4) {
                self.structs.push(0);
            }
        }

        fn begin_node(&mut self, name: &str) -> &mut Self {
            self.token(Self::BEGIN_NODE);
            self.structs.extend_from_slice(name.as_bytes());
            self.structs.push(0);
            self.pad_structs();
            self
        }

        fn end_node(&mut self) -> &mut Self {
            self.token(Self::END_NODE);
            self
        }

        fn prop(&mut self, name: &str, value: &[u8]) -> &mut Self {
            let off = self.name_off(name);
            self.token(Self::PROP);
            self.structs
                .extend_from_slice(&(value.len() as u32).to_be_bytes());
            self.structs.extend_from_slice(&off.to_be_bytes());
            self.structs.extend_from_slice(value);
            self.pad_structs();
            self
        }

        fn finish(mut self) -> Vec<u8> {
            self.token(Self::END);
            // Header, then the reservation block at an 8-byte boundary, then
            // the struct block and the string table, in the order `dtc`
            // emits them.
            let off_mem_rsvmap = FDT_HEADER_SIZE;
            let rsvmap = [0u8; 16]; // one entry of two zero `u64`s: the end
            let off_dt_struct = off_mem_rsvmap + rsvmap.len();
            let off_dt_strings = off_dt_struct + self.structs.len();
            let totalsize = off_dt_strings + self.strings.len();

            let mut blob = cells(&[
                FDT_MAGIC,
                totalsize as u32,
                off_dt_struct as u32,
                off_dt_strings as u32,
                off_mem_rsvmap as u32,
                17,
                16,
                0,
                self.strings.len() as u32,
                self.structs.len() as u32,
            ]);
            assert_eq!(blob.len(), FDT_HEADER_SIZE);
            blob.extend_from_slice(&rsvmap);
            blob.extend_from_slice(&self.structs);
            blob.extend_from_slice(&self.strings);
            assert_eq!(blob.len(), totalsize);
            blob
        }
    }

    /// The shape of the `virt` machine's own blob, cut down to two nodes.
    fn a_real_blob() -> Vec<u8> {
        let mut fdt = Fdt::new();
        fdt.begin_node("")
            .prop("#address-cells", &cells(&[2]))
            .prop("#size-cells", &cells(&[2]))
            .prop("compatible", &strs(&["linux,dummy-virt"]))
            .begin_node("chosen")
            .prop("bootargs", &strs(&["LOG=info smp=off"]))
            .end_node()
            .begin_node("pl011@9000000")
            .prop("compatible", &strs(&["arm,pl011", "arm,primecell"]))
            .prop("reg", &cells(&[0, 0x0900_0000, 0, 0x1000]))
            .prop("interrupts", &cells(&[0, 1, 4]))
            .end_node()
            .end_node();
        fdt.finish()
    }

    #[test]
    fn a_whole_blob_laid_out_the_way_the_spec_says_passes_the_check() {
        // Hand-made headers only prove the check is self-consistent. This one
        // proves the ten words are where I think they are: if any offset were
        // wrong, every real DTB --- the one the riscv boot already reads ---
        // would start being refused.
        let blob = a_real_blob();
        assert_eq!(check_fdt_header(&blob), Ok(blob.len()));
    }

    #[test]
    fn a_blob_in_memory_loads_through_the_same_path_the_kernel_takes() {
        // `Devicetree::from` and its two raw slices, run on an address that
        // really is a blob. It is the only test that covers the unsafe code.
        let blob = a_real_blob();
        let dt = Devicetree::from(blob.as_ptr() as VirtAddr).unwrap();
        assert_eq!(dt.bootargs(), Some("LOG=info smp=off"));
        let mut uarts = Vec::new();
        dt.walk(&mut |node, comp, _| {
            if comp.contains("arm,pl011") {
                uarts.push(node.name.clone());
            }
        });
        assert_eq!(uarts, vec!["pl011@9000000".to_string()]);
    }

    #[test]
    fn an_address_whose_header_does_not_add_up_is_refused_before_anything_is_read() {
        // The same path with one field broken: `Devicetree::from` must come
        // back with an error instead of building a slice out of the second
        // word. The blob is a full one, so the only thing stopping it is the
        // check.
        let mut blob = a_real_blob();
        blob[0..4].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(
            Devicetree::from(blob.as_ptr() as VirtAddr).err(),
            Some(DeviceError::InvalidParam)
        );
        // And a `totalsize` of 3 GiB, which is what the old loader would have
        // taken as the length of a slice.
        let mut blob = a_real_blob();
        blob[4..8].copy_from_slice(&0xC000_0000u32.to_be_bytes());
        assert_eq!(
            Devicetree::from(blob.as_ptr() as VirtAddr).err(),
            Some(DeviceError::InvalidParam)
        );
    }

    // ---- `interrupts` vs `interrupts-extended` -----------------------------

    #[test]
    fn a_plain_interrupts_list_keeps_every_specifier_it_has() {
        let n = leaf(
            "mmc@4020000",
            vec![prop("interrupts", cells(&[35, 36, 37]))],
        );
        let props = InheritProps {
            interrupt_parent: 7,
            ..Default::default()
        };
        // The parent is named once, off to the side: putting it in front of
        // the list would make cell 36 look like a second phandle.
        assert_eq!(
            parse_interrupts(&n, &props).unwrap(),
            InterruptsProp {
                parent: Some(7),
                specs: vec![35, 36, 37],
            }
        );
    }

    #[test]
    fn an_extended_list_carries_its_own_parents() {
        let n = leaf(
            "serial@2500000",
            vec![prop("interrupts-extended", cells(&[7, 35, 9, 4]))],
        );
        let props = InheritProps {
            interrupt_parent: 7,
            ..Default::default()
        };
        assert_eq!(
            parse_interrupts(&n, &props).unwrap(),
            InterruptsProp {
                parent: None,
                specs: vec![7, 35, 9, 4],
            }
        );
    }

    #[test]
    fn extended_wins_over_plain_when_a_node_has_both() {
        let n = leaf(
            "serial@2500000",
            vec![
                prop("interrupts", cells(&[99])),
                prop("interrupts-extended", cells(&[7, 35])),
            ],
        );
        let props = InheritProps {
            interrupt_parent: 7,
            ..Default::default()
        };
        let got = parse_interrupts(&n, &props).unwrap();
        assert_eq!(got.parent, None);
        assert_eq!(got.specs, vec![7, 35]);
    }

    #[test]
    fn interrupts_without_a_parent_ask_for_nothing() {
        let n = leaf("serial@2500000", vec![prop("interrupts", cells(&[35]))]);
        // phandle 0 is not a phandle: with no `interrupt-parent` anywhere up
        // the tree there is nobody to register with.
        let got = parse_interrupts(&n, &InheritProps::default()).unwrap();
        assert!(got.is_empty());
        assert_eq!(got.parent, None);
        assert_eq!(got.first_irq(), None);
    }

    #[test]
    fn the_first_interrupt_number_sits_past_the_phandle_only_in_the_extended_form() {
        let plain = InterruptsProp {
            parent: Some(7),
            specs: vec![35, 36],
        };
        let extended = InterruptsProp {
            parent: None,
            specs: vec![7, 35],
        };
        assert_eq!(plain.first_irq(), Some(35));
        assert_eq!(extended.first_irq(), Some(35));
        // An extended list with a parent and no arguments names no number.
        assert_eq!(
            InterruptsProp {
                parent: None,
                specs: vec![7],
            }
            .first_irq(),
            None
        );
        assert_eq!(InterruptsProp::default().first_irq(), None);
    }

    // ---- inherited cells --------------------------------------------------

    #[test]
    fn the_cells_a_node_is_read_with_are_its_parents_not_its_own() {
        let child = leaf(
            "serial@2500000",
            vec![
                prop("compatible", strs(&["ns16550a"])),
                // Its own `#address-cells` describes *its* children, and must
                // not be used to read its own `reg`.
                prop("#address-cells", cells(&[1])),
                prop("reg", cells(&[0, 0x0250_0000, 0, 0x400])),
            ],
        );
        let root = node(
            "",
            vec![
                prop("#address-cells", cells(&[2])),
                prop("#size-cells", cells(&[2])),
            ],
            vec![child],
        );
        let dt = Devicetree::from_root(root);

        let mut seen = Vec::new();
        dt.walk(&mut |n, _comp, props| seen.push((n.name.clone(), props.clone())));
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].1.parent_address_cells, 2);
        assert_eq!(seen[0].1.parent_size_cells, 2);
        assert_eq!(
            parse_reg(&dt.0.root.children[0], &seen[0].1).unwrap(),
            (0x0250_0000, 0x400)
        );
    }

    #[test]
    fn a_node_that_declares_no_cells_is_read_as_declaring_zero() {
        // The specification's defaults are 2 and 1, not 0 and 0; this pins
        // what the code does today, which is to read such a node's children's
        // `reg` as `(0, 0)` rather than guess. Every device tree that reaches
        // this kernel declares them on `/` and on `/soc`.
        let grandchild = leaf(
            "serial@2500000",
            vec![
                prop("compatible", strs(&["ns16550a"])),
                prop("reg", cells(&[0x0250_0000, 0x400])),
            ],
        );
        let soc = node(
            "soc",
            vec![prop("compatible", strs(&["simple-bus"]))],
            vec![grandchild],
        );
        let root = node(
            "",
            vec![
                prop("#address-cells", cells(&[1])),
                prop("#size-cells", cells(&[1])),
            ],
            vec![soc],
        );
        let dt = Devicetree::from_root(root);

        let mut seen = Vec::new();
        dt.walk(&mut |n, _comp, props| seen.push((n.name.clone(), props.clone())));
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[1].0, "serial@2500000");
        assert_eq!(seen[1].1.parent_address_cells, 0);
        assert_eq!(seen[1].1.parent_size_cells, 0);
    }

    #[test]
    fn an_interrupt_parent_reaches_every_node_below_it() {
        let deep = leaf(
            "serial@2500000",
            vec![
                prop("compatible", strs(&["ns16550a"])),
                prop("interrupts", cells(&[35])),
            ],
        );
        let closer = leaf(
            "eth@4500000",
            vec![
                prop("compatible", strs(&["allwinner,sunxi-gmac"])),
                // Its own property wins over the inherited one.
                prop("interrupt-parent", cells(&[9])),
                prop("interrupts", cells(&[62])),
            ],
        );
        let soc = node(
            "soc",
            vec![
                prop("compatible", strs(&["simple-bus"])),
                prop("interrupt-parent", cells(&[7])),
            ],
            vec![deep, closer],
        );
        let root = node("", Vec::new(), vec![soc]);
        let dt = Devicetree::from_root(root);

        let mut seen = Vec::new();
        dt.walk(&mut |n, _comp, props| {
            seen.push((n.name.clone(), parse_interrupts(n, props).unwrap()))
        });
        let by_name = |want: &str| {
            seen.iter()
                .find(|(name, _)| name == want)
                .map(|(_, p)| p.clone())
                .unwrap()
        };
        assert_eq!(by_name("serial@2500000").parent, Some(7));
        assert_eq!(by_name("eth@4500000").parent, Some(9));
    }

    // ---- `reg` ------------------------------------------------------------

    #[test]
    fn a_reg_shorter_than_the_cells_it_is_read_with_is_an_error() {
        let n = leaf("serial@2500000", vec![prop("reg", cells(&[0x0250_0000]))]);
        let props = InheritProps {
            parent_address_cells: 2,
            parent_size_cells: 1,
            ..Default::default()
        };
        // Not a panic: the address would be sliced out past the end of the
        // property, and the size out past the end of that.
        assert_eq!(parse_reg(&n, &props), Err(DeviceError::InvalidParam));
    }

    #[test]
    fn a_reg_with_no_size_cells_has_no_size() {
        let n = leaf("serial@2500000", vec![prop("reg", cells(&[0x0250_0000]))]);
        let props = InheritProps {
            parent_address_cells: 1,
            parent_size_cells: 0,
            ..Default::default()
        };
        assert_eq!(parse_reg(&n, &props).unwrap(), (0x0250_0000, 0));
    }

    #[test]
    fn a_missing_reg_is_an_error() {
        let n = leaf(
            "serial@2500000",
            vec![prop("compatible", strs(&["ns16550a"]))],
        );
        assert_eq!(
            parse_reg(&n, &InheritProps::default()),
            Err(DeviceError::InvalidParam)
        );
    }

    #[test]
    fn three_cells_of_address_only_pass_when_the_top_one_is_empty() {
        let props = InheritProps {
            parent_address_cells: 3,
            parent_size_cells: 2,
            ..Default::default()
        };
        // The high cell is zero, so nothing is lost by squeezing it into 64
        // bits: this is the shape a PCI-style address has.
        let ok = leaf(
            "dev",
            vec![prop("reg", cells(&[0, 0x40, 0x1000, 0, 0x2000]))],
        );
        assert_eq!(parse_reg(&ok, &props).unwrap(), (0x40_0000_1000, 0x2000));
        // With the high cell set the address does not fit, and returning its
        // low half would map a real address that is not the one in the blob.
        let lost = leaf(
            "dev",
            vec![prop("reg", cells(&[1, 0x40, 0x1000, 0, 0x2000]))],
        );
        assert_eq!(parse_reg(&lost, &props), Err(DeviceError::InvalidParam));
    }

    // ---- `/memory` --------------------------------------------------------

    #[test]
    fn a_memory_region_that_runs_off_the_end_of_the_address_space_is_dropped() {
        let bad = leaf(
            "memory@0",
            vec![
                prop("device_type", strs(&["memory"])),
                prop("reg", cells(&[0xffff_ffff, 0xffff_0000, 0, 0x8000_0000])),
            ],
        );
        let good = leaf(
            "memory@40000000",
            vec![
                prop("device_type", strs(&["memory"])),
                prop("reg", cells(&[0, 0x4000_0000, 0, 0x1000_0000])),
            ],
        );
        let root = node(
            "",
            vec![
                prop("#address-cells", cells(&[2])),
                prop("#size-cells", cells(&[2])),
            ],
            vec![bad, good],
        );
        // The sum used to panic the kernel in a debug build and wrap round in
        // a release one; the good region behind it still comes back.
        assert_eq!(
            Devicetree::from_root(root).memory_regions().unwrap(),
            vec![0x4000_0000..0x5000_0000]
        );
    }

    #[test]
    fn a_memory_node_is_one_named_memory_or_one_that_says_it_is() {
        let by_name = leaf(
            "memory@40000000",
            vec![prop("reg", cells(&[0x4000_0000, 0x1000]))],
        );
        let by_type = leaf(
            "ram",
            vec![
                prop("device_type", strs(&["memory"])),
                prop("reg", cells(&[0x8000_0000, 0x1000])),
            ],
        );
        let neither = leaf(
            "serial@2500000",
            vec![
                prop("compatible", strs(&["ns16550a"])),
                prop("reg", cells(&[0x0250_0000, 0x400])),
            ],
        );
        let root = node(
            "",
            vec![
                prop("#address-cells", cells(&[1])),
                prop("#size-cells", cells(&[1])),
            ],
            vec![by_name, by_type, neither],
        );
        assert_eq!(
            Devicetree::from_root(root).memory_regions().unwrap(),
            vec![0x4000_0000..0x4000_1000, 0x8000_0000..0x8000_1000]
        );
    }

    // ---- `/chosen` and `/cpus` --------------------------------------------

    #[test]
    fn an_initrd_that_ends_before_it_starts_is_not_a_region() {
        let chosen = |start: u32, end: u32| {
            node(
                "",
                Vec::new(),
                vec![leaf(
                    "chosen",
                    vec![
                        prop("linux,initrd-start", cells(&[start])),
                        prop("linux,initrd-end", cells(&[end])),
                    ],
                )],
            )
        };
        assert_eq!(
            Devicetree::from_root(chosen(0x8400_0000, 0x8500_0000)).initrd_region(),
            Some(0x8400_0000..0x8500_0000)
        );
        // `init_ram_disk` builds a slice at `start` of this range's length, so
        // a backwards one is a slice nobody asked for.
        assert_eq!(
            Devicetree::from_root(chosen(0x8500_0000, 0x8400_0000)).initrd_region(),
            None
        );
    }

    #[test]
    fn the_command_line_and_the_clock_come_out_of_their_own_nodes() {
        let root = node(
            "",
            Vec::new(),
            vec![
                leaf("chosen", vec![prop("bootargs", strs(&["LOG=debug"]))]),
                leaf(
                    "cpus",
                    vec![prop("timebase-frequency", cells(&[24_000_000]))],
                ),
            ],
        );
        let dt = Devicetree::from_root(root);
        assert_eq!(dt.bootargs(), Some("LOG=debug"));
        assert_eq!(dt.timebase_frequency(), Some(24_000_000));
        assert_eq!(dt.initrd_region(), None);

        // A tree without them answers `None` rather than taking the boot down.
        let bare = Devicetree::from_root(node("", Vec::new(), Vec::new()));
        assert_eq!(bare.bootargs(), None);
        assert_eq!(bare.timebase_frequency(), None);
        assert_eq!(bare.initrd_region(), None);
        assert_eq!(bare.memory_regions().unwrap(), Vec::new());
    }

    #[test]
    fn a_blob_that_is_not_a_blob_is_an_error_not_a_crash() {
        // The header carries its own length, which `load_from_raw_pointer`
        // reads before anything else; this one is honest about it and wrong
        // about everything after.
        let mut blob = [0u8; 64];
        blob[4..8].copy_from_slice(&64u32.to_be_bytes());
        assert_eq!(
            Devicetree::from(blob.as_ptr() as usize).err(),
            Some(DeviceError::InvalidParam)
        );
    }
}

#[cfg(test)]
mod ranges_tests {
    use super::*;
    use alloc::{
        string::{String, ToString},
        vec,
        vec::Vec,
    };

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

    fn node(name: &str, props: Vec<(String, Vec<u8>)>, children: Vec<Node>) -> Node {
        Node {
            name: name.to_string(),
            props,
            children,
        }
    }

    /// El trozo del arbol de la Raspberry Pi 5 que importa, de
    /// `bcm2712.dtsi`, `bcm2712-rpi-5-b.dts` y `rp1.dtsi`.
    ///
    /// Tres niveles, y los tres hacen algo distinto:
    ///
    /// ```text
    /// / { #address-cells = 2; #size-cells = 2;        // sin ranges
    ///     axi { ranges;                                // identidad
    ///         pcie@120000 { device_type = "pci";        // 3 celdas, PCI
    ///             #address-cells = 3; #size-cells = 2;
    ///             ranges = <0x02000000 0 0  0x1f 0  0x0 0xfffffffc>,
    ///                      <0x43000000 0x4 0  0x1c 0  0x3 0>;
    ///             rp1 { #address-cells = 2; #size-cells = 2;
    ///                 ranges = <0xc0 0x40000000  0x02000000 0 0  0 0x410000>;
    ///                 ethernet@100000 { reg = <0xc0 0x40100000  0 0x4000>; };
    ///             };
    ///         };
    ///     };
    /// };
    /// ```
    fn arbol_de_la_pi5() -> Devicetree {
        let gem = node(
            "ethernet@100000",
            vec![
                prop("compatible", strs(&["raspberrypi,rp1-gem", "cdns,macb"])),
                prop("reg", cells(&[0xc0, 0x4010_0000, 0x0, 0x4000])),
            ],
            vec![],
        );
        let rp1 = node(
            "rp1",
            vec![
                prop("compatible", strs(&["simple-bus"])),
                prop("#address-cells", cells(&[2])),
                prop("#size-cells", cells(&[2])),
                // Una sola ventana: el espacio de periféricos del RP1, que
                // sale al espacio de memoria de 32 bits del PCIe.
                prop(
                    "ranges",
                    cells(&[0xc0, 0x4000_0000, 0x0200_0000, 0x0, 0x0, 0x0, 0x41_0000]),
                ),
            ],
            vec![gem],
        );
        let pcie = node(
            "pcie@120000",
            vec![
                prop("compatible", strs(&["brcm,bcm2712-pcie"])),
                prop("device_type", strs(&["pci"])),
                prop("#address-cells", cells(&[3])),
                prop("#size-cells", cells(&[2])),
                prop("reg", cells(&[0x10, 0x0012_0000, 0x0, 0x9310])),
                prop(
                    "ranges",
                    cells(&[
                        // 4 GiB de memoria de 32 bits en PCIe 0x0 -> CPU 0x1f_00000000
                        0x0200_0000,
                        0x0,
                        0x0,
                        0x1f,
                        0x0,
                        0x0,
                        0xffff_fffc,
                        // 12 GiB de memoria de 64 bits en PCIe 0x4_00000000 -> CPU 0x1c_00000000
                        0x4300_0000,
                        0x4,
                        0x0,
                        0x1c,
                        0x0,
                        0x3,
                        0x0,
                    ]),
                ),
            ],
            vec![rp1],
        );
        let axi = node(
            "axi",
            vec![
                prop("compatible", strs(&["simple-bus"])),
                prop("#address-cells", cells(&[2])),
                prop("#size-cells", cells(&[2])),
                // `ranges;` a secas: la identidad.
                prop("ranges", Vec::new()),
            ],
            vec![pcie],
        );
        Devicetree::from_root(node(
            "",
            vec![
                prop("#address-cells", cells(&[2])),
                prop("#size-cells", cells(&[2])),
            ],
            vec![axi],
        ))
    }

    /// Lo que el `reg` de un nodo da, buscandolo por nombre en el paseo.
    fn reg_de(dt: &Devicetree, nombre: &str) -> Option<DeviceResult<(u64, u64)>> {
        let mut visto = None;
        dt.walk(&mut |n, _comp, props| {
            if n.name == nombre {
                visto = Some(parse_reg(n, props));
            }
        });
        visto
    }

    /// **La direccion de la Pi 5**, atravesando los dos puentes.
    ///
    /// `0xc0_40100000` del bus del RP1 pasa por su `ranges` al espacio de
    /// memoria de 32 bits del PCIe, en `0x0_00100000`, y por el `ranges` del
    /// puente a la direccion de CPU `0x1f_00100000`. Es el numero que hay que
    /// mapear, y leer el `reg` tal cual mapea otra cosa.
    #[test]
    fn the_gem_of_the_pi5_lands_at_the_cpu_address_the_two_bridges_say() {
        let dt = arbol_de_la_pi5();
        assert_eq!(
            reg_de(&dt, "ethernet@100000"),
            Some(Ok((0x1f_0010_0000, 0x4000)))
        );
    }

    /// Y el `reg` del propio puente **no** se traduce: esta en el bus de
    /// arriba, que es la identidad.
    ///
    /// Es la mitad que se olvida: la traduccion que un nodo declara es para
    /// sus hijos, no para si mismo. Aplicarsela a su propio `reg` mandaria la
    /// ventana de registros del puente al espacio de PCI que el puente
    /// todavia no ha levantado.
    #[test]
    fn the_bridges_own_window_is_not_translated_by_its_own_ranges() {
        let dt = arbol_de_la_pi5();
        assert_eq!(
            reg_de(&dt, "pcie@120000"),
            Some(Ok((0x10_0012_0000, 0x9310)))
        );
    }

    /// Un arbol **sin ningun `ranges`** sigue dando la direccion del `reg`.
    ///
    /// Es lo que son la `virt` de QEMU y la Pi 4, y lo que hacia Eclipse hasta
    /// ahora: si esto cambiara, esas placas se quedarian sin consola.
    #[test]
    fn a_tree_with_no_ranges_anywhere_still_gives_the_address_in_the_reg() {
        let dt = Devicetree::from_root(node(
            "",
            vec![
                prop("#address-cells", cells(&[2])),
                prop("#size-cells", cells(&[2])),
            ],
            vec![node(
                "soc",
                vec![
                    prop("compatible", strs(&["simple-bus"])),
                    prop("#address-cells", cells(&[2])),
                    prop("#size-cells", cells(&[2])),
                ],
                vec![node(
                    "serial@9000000",
                    vec![
                        prop("compatible", strs(&["arm,pl011"])),
                        prop("reg", cells(&[0x0, 0x900_0000, 0x0, 0x1000])),
                    ],
                    vec![],
                )],
            )],
        ));
        assert_eq!(
            reg_de(&dt, "serial@9000000"),
            Some(Ok((0x900_0000, 0x1000)))
        );
    }

    // -----------------------------------------------------------------------
    // El formato de PCI
    // -----------------------------------------------------------------------

    /// La primera celda de una direccion de PCI **son banderas**, no
    /// direccion.
    ///
    /// `0x02000000` metido en el numero daria `0x02000000_00000000`, y
    /// `from_cells` ni eso: refusa tres celdas cuya primera no sea cero. Las
    /// dos formas pierden la ventana.
    #[test]
    fn the_first_cell_of_a_pci_address_is_flags_and_not_part_of_the_number() {
        assert_eq!(
            AddrFormat::Pci.split(&[0x0200_0000, 0x0, 0x0], 3),
            Some((2, 0))
        );
        assert_eq!(
            AddrFormat::Pci.split(&[0x4300_0000, 0x4, 0x0], 3),
            Some((3, 0x4_0000_0000))
        );
        // Y en un bus corriente las tres celdas son el numero --- que es por
        // lo que `from_cells` refusa esta: no cabe en 64 bits.
        assert!(AddrFormat::Plain
            .split(&[0x0200_0000, 0x0, 0x0], 3)
            .is_none());
    }

    /// El codigo de espacio son **los bits 24 y 25**, y nada mas.
    ///
    /// El 30 (prefetchable) y el 31 (relocatable) no entran: dos ventanas del
    /// mismo espacio pueden diferir en ellos, y comparar la palabra entera
    /// dejaria la ventana sin casar.
    #[test]
    fn the_space_code_is_two_bits_and_the_other_flags_do_not_count() {
        assert_eq!(pci_space(0x0000_0000), 0, "configuracion");
        assert_eq!(pci_space(0x0100_0000), 1, "entrada/salida");
        assert_eq!(pci_space(0x0200_0000), 2, "memoria de 32 bits");
        assert_eq!(pci_space(0x0300_0000), 3, "memoria de 64 bits");
        // `0x43000000` es memoria de 64 bits **prefetchable**, que es la
        // segunda ventana del puente del BCM2712.
        assert_eq!(pci_space(0x4300_0000), 3);
        assert_eq!(
            pci_space(0x8200_0000),
            2,
            "relocatable no cambia el espacio"
        );
        // Los bits 26 a 29 estan reservados. Un arbol que ponga alguno no
        // cambia de espacio: solo los bits 24 y 25 lo dicen, y meter el 26 en
        // la cuenta convertiria la memoria de 32 bits en un espacio que no
        // existe y dejaria la ventana sin casar.
        assert_eq!(pci_space(0x0600_0000), 2, "el bit 26 entro en el espacio");
        assert_eq!(pci_space(0x3f00_0000), 3, "los bits 26-29 entraron");
    }

    /// Y el espacio **se casa**: un bus de PCI tiene una direccion cero en
    /// cada espacio, y la de uno no vale para el otro.
    ///
    /// En el puente del BCM2712 la ventana de 32 bits empieza en la direccion
    /// de PCIe 0 y la de 64 bits en `0x4_00000000`. Si no se casara el
    /// espacio, el `0x0` de la de 64 bits --- que no existe --- caeria en la
    /// de 32 y daria una direccion de CPU que no es.
    #[test]
    fn a_zero_in_one_pci_space_is_not_the_zero_of_another() {
        let dt = arbol_de_la_pi5();
        let mut bus = None;
        dt.walk(&mut |n, _comp, props| {
            if n.name == "rp1" {
                // La traduccion que el nodo `rp1` USA para su propio `reg`,
                // que es la del puente de PCIe.
                bus = Some(props.bus.clone());
            }
        });
        let bus = bus.expect("el nodo rp1 esta en el arbol");
        // Memoria de 32 bits en 0 -> 0x1f_00000000.
        assert_eq!(bus.translate(2, 0, 0x1000), Some(0x1f_0000_0000));
        // Memoria de 64 bits en 0x4_00000000 -> 0x1c_00000000.
        assert_eq!(
            bus.translate(3, 0x4_0000_0000, 0x1000),
            Some(0x1c_0000_0000)
        );
        // Y el cero de la de 64 bits no esta en ninguna ventana.
        assert_eq!(bus.translate(3, 0, 0x1000), None);
        // Ni el espacio de configuracion, que este puente no saca por
        // `ranges`.
        assert_eq!(bus.translate(0, 0, 0x1000), None);
    }

    // -----------------------------------------------------------------------
    // Los bordes
    // -----------------------------------------------------------------------

    /// Una region que **se sale por arriba** de su ventana no se traduce.
    ///
    /// Traducir solo su primer byte daria un mapeo que funciona hasta la
    /// primera pagina de mas, y entonces escribe en lo que haya detras de la
    /// ventana.
    #[test]
    fn a_region_that_runs_off_the_end_of_its_window_is_refused() {
        let dt = arbol_de_la_pi5();
        let mut bus = None;
        dt.walk(&mut |n, _comp, props| {
            if n.name == "ethernet@100000" {
                bus = Some(props.bus.clone());
            }
        });
        let bus = bus.expect("el GEM esta en el arbol");
        // La ventana del RP1 son 0x410000 bytes desde 0xc0_40000000.
        assert_eq!(
            bus.translate(0, 0xc0_4040_f000, 0x1000),
            Some(0x1f_0040_f000),
            "el ultimo trozo de la ventana tiene que entrar"
        );
        assert_eq!(
            bus.translate(0, 0xc0_4040_f000, 0x2000),
            None,
            "una region que se sale se tradujo igual"
        );
        assert_eq!(
            bus.translate(0, 0xc0_4041_0000, 0x1000),
            None,
            "el primer byte de despues de la ventana se tradujo"
        );
    }

    /// Un `ranges` cuyas celdas no son multiplo del paso **no se lee a
    /// medias**: se queda la traduccion de arriba.
    ///
    /// Leer un triple incompleto daria una ventana con un tamaño o una
    /// direccion sacados de la nada.
    #[test]
    fn a_ranges_whose_cells_do_not_divide_into_triples_is_not_read_in_halves() {
        let padre = BusTranslation::default();
        let nodo = node(
            "bus",
            vec![
                prop("#address-cells", cells(&[2])),
                prop("#size-cells", cells(&[2])),
                // Cinco celdas donde hacen falta seis.
                prop("ranges", cells(&[0x0, 0x1000, 0x0, 0x2000, 0x0])),
            ],
            vec![],
        );
        let hijo = padre.compose_child(&nodo, AddrFormat::Plain, 2, 2, AddrFormat::Plain, 2);
        assert!(hijo.is_identity(), "se leyo un `ranges` roto");
    }

    /// Y una ventana que el bus de arriba **no sabe traducir** no se guarda
    /// como si su direccion de padre fuera de CPU.
    ///
    /// Eso es exactamente el fallo que todo esto viene a arreglar, un nivel
    /// mas arriba. Lo que queda es un bus que no traduce nada, que es la
    /// respuesta honrada: el nodo da `Err` en vez de una direccion inventada.
    #[test]
    fn a_window_the_bus_above_cannot_translate_is_not_taken_for_a_cpu_address() {
        // Un padre que solo traduce el espacio 2 desde 0x1000.
        let padre = BusTranslation::default().compose_child(
            &node(
                "arriba",
                vec![
                    prop("#address-cells", cells(&[2])),
                    prop("#size-cells", cells(&[2])),
                    prop("ranges", cells(&[0x0, 0x1000, 0x0, 0x9000, 0x0, 0x1000])),
                ],
                vec![],
            ),
            AddrFormat::Plain,
            2,
            2,
            AddrFormat::Plain,
            2,
        );
        assert_eq!(padre.translate(0, 0x1000, 0x1000), Some(0x9000));

        // Y un hijo cuya unica ventana sale a una direccion que el padre no
        // cubre.
        let nodo = node(
            "abajo",
            vec![
                prop("#address-cells", cells(&[2])),
                prop("#size-cells", cells(&[2])),
                prop("ranges", cells(&[0x0, 0x0, 0x0, 0xdead_0000, 0x0, 0x1000])),
            ],
            vec![],
        );
        let hijo = padre.compose_child(&nodo, AddrFormat::Plain, 2, 2, AddrFormat::Plain, 2);
        assert!(!hijo.is_identity(), "se quedo la identidad, que es mentir");
        assert_eq!(hijo.translate(0, 0, 0x1000), None);
    }

    /// `ranges;` a secas **es la identidad**, no la ausencia de traduccion.
    ///
    /// Es lo que declara el bus de arriba del puente en el BCM2712, y lo que
    /// pasa entre el `0x10_00120000` del `reg` del puente y la direccion de
    /// CPU que hay que mapear: son la misma.
    #[test]
    fn an_empty_ranges_property_is_the_identity() {
        let padre = BusTranslation::default();
        let nodo = node(
            "axi",
            vec![
                prop("#address-cells", cells(&[2])),
                prop("#size-cells", cells(&[2])),
                prop("ranges", Vec::new()),
            ],
            vec![],
        );
        let hijo = padre.compose_child(&nodo, AddrFormat::Plain, 2, 2, AddrFormat::Plain, 2);
        assert!(hijo.is_identity());
        assert_eq!(
            hijo.translate(0, 0x10_0012_0000, 0x9310),
            Some(0x10_0012_0000)
        );
    }

    /// Un bus **sin `ranges`** debajo de un puente **sigue traduciendo por el
    /// puente**.
    ///
    /// Es el caso de cualquier `simple-bus` colgado de un PCIe, y la razon por
    /// la que la ausencia de `ranges` no puede volver a la identidad: eso
    /// diria que las direcciones de ese bus son de CPU, que es justo lo que el
    /// puente de arriba niega. El `rp1` de la Pi 5 llega a tener `ranges`
    /// porque su `dtsi` lo pone; un nodo intermedio cualquiera no.
    #[test]
    fn a_bus_with_no_ranges_under_a_bridge_still_translates_through_the_bridge() {
        let dispositivo = node(
            "serial@1000",
            vec![
                prop("compatible", strs(&["arm,pl011"])),
                prop("reg", cells(&[0xc0, 0x4010_0000, 0x0, 0x1000])),
            ],
            vec![],
        );
        // Un bus que no declara `ranges`, metido entre el `rp1` y el
        // dispositivo.
        let intermedio = node(
            "peripherals",
            vec![
                prop("compatible", strs(&["simple-bus"])),
                prop("#address-cells", cells(&[2])),
                prop("#size-cells", cells(&[2])),
            ],
            vec![dispositivo],
        );
        let mut dt = arbol_de_la_pi5();
        // Colgarlo del `rp1`, que es el que trae la ventana de los
        // perifericos.
        let raiz = dt.root_mut();
        raiz.children[0].children[0].children[0]
            .children
            .push(intermedio);
        assert_eq!(
            reg_de(&dt, "serial@1000"),
            Some(Ok((0x1f_0010_0000, 0x1000))),
            "el bus sin `ranges` perdio la traduccion del puente"
        );
    }

    /// El `reg` de un dispositivo que cuelga **directamente del PCIe** se lee
    /// como una direccion de PCI, y su espacio decide la ventana.
    ///
    /// Si se tradujera siempre con el espacio cero, un dispositivo en la
    /// ventana de 64 bits caeria en la de 32 y daria una direccion de CPU que
    /// no es la suya: `0x1f_…` en vez de `0x1c_…`.
    #[test]
    fn a_device_hanging_off_the_bridge_is_translated_in_the_space_its_reg_names() {
        let mut dt = arbol_de_la_pi5();
        let raiz = dt.root_mut();
        // `0x43000000` es memoria de 64 bits prefetchable, y la ventana del
        // puente para ese espacio empieza en la direccion de PCIe
        // `0x4_00000000`.
        raiz.children[0].children[0].children.push(node(
            "device@400000000",
            vec![
                prop("compatible", strs(&["un,dispositivo"])),
                prop("reg", cells(&[0x4300_0000, 0x4, 0x0001_0000, 0x0, 0x1000])),
            ],
            vec![],
        ));
        assert_eq!(
            reg_de(&dt, "device@400000000"),
            Some(Ok((0x1c_0001_0000, 0x1000)))
        );
    }

    /// Un nodo sin `device_type` es un bus corriente, y `device_type = "pci"`
    /// el unico que cambia el formato.
    #[test]
    fn only_a_node_that_says_pci_reads_its_addresses_as_pci() {
        assert_eq!(
            AddrFormat::of(&node("bus", vec![], vec![])),
            AddrFormat::Plain
        );
        assert_eq!(
            AddrFormat::of(&node(
                "memory@0",
                vec![prop("device_type", strs(&["memory"]))],
                vec![]
            )),
            AddrFormat::Plain
        );
        assert_eq!(
            AddrFormat::of(&node(
                "pcie@120000",
                vec![prop("device_type", strs(&["pci"]))],
                vec![]
            )),
            AddrFormat::Pci
        );
    }
}
