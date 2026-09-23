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

/// Some properties inherited from ancestor nodes.
///
/// About the notion: cell, see <https://elinux.org/Device_Tree_Usage#How_Addressing_Works>.
#[derive(Clone, Copy, Debug, Default)]
pub struct InheritProps {
    /// The `#address-cells` property of its parent node.
    pub parent_address_cells: u32,
    /// The `#size-cells` property of its parent node.
    pub parent_size_cells: u32,
    /// The `interrupt-parent` property of the node. If don't have, inherit from
    /// its parent node.
    pub interrupt_parent: u32,
}

impl Devicetree {
    /// Load the device tree blob from the given virtual address.
    pub fn from(dtb_base_vaddr: VirtAddr) -> DeviceResult<Self> {
        info!("Loading device tree blob from {:#x}", dtb_base_vaddr);
        match unsafe { DeviceTreeInner::load_from_raw_pointer(dtb_base_vaddr as *const _) } {
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

        props.parent_address_cells = node.prop_u32("#address-cells").unwrap_or(0);
        props.parent_size_cells = node.prop_u32("#size-cells").unwrap_or(0);

        // DFS
        for child in node.children.iter() {
            self.walk_inner(child, props, device_node_op);
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
pub fn parse_reg(node: &Node, props: &InheritProps) -> DeviceResult<(u64, u64)> {
    let cells = node.prop_cells("reg")?;
    let addr = from_cells(&cells, props.parent_address_cells)?;
    let size = from_cells(
        &cells[props.parent_address_cells as usize..],
        props.parent_size_cells,
    )?;
    Ok((addr, size))
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
        dt.walk(&mut |n, _comp, props| seen.push((n.name.clone(), *props)));
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
        dt.walk(&mut |n, _comp, props| seen.push((n.name.clone(), *props)));
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
        let mut blob = vec![0u8; 64];
        blob[4..8].copy_from_slice(&64u32.to_be_bytes());
        assert_eq!(
            Devicetree::from(blob.as_ptr() as usize).err(),
            Some(DeviceError::InvalidParam)
        );
    }
}
