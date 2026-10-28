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
        let mut blob = [0u8; 64];
        blob[4..8].copy_from_slice(&64u32.to_be_bytes());
        assert_eq!(
            Devicetree::from(blob.as_ptr() as usize).err(),
            Some(DeviceError::InvalidParam)
        );
    }
}
