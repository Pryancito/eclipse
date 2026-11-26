//! A PCIe host bridge as a device tree describes it (`pci-host-ecam-generic`,
//! QEMU `virt` on aarch64): where its configuration space is, which buses
//! it holds, and which interrupt line each function's INTx pin raises.
//!
//! The arithmetic only: the caller maps the window and scans it.

use alloc::vec::Vec;

use device_tree::Node;

/// What the host bridge node gives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PciHost {
    /// The ECAM window: base, length.
    pub ecam: (u64, u64),
    /// `bus-range`, inclusive.
    pub buses: (u8, u8),
    /// `interrupt-map-mask`: unit address (3 cells) and pin.
    pub mask: [u32; 4],
    pub intx: Vec<Intx>,
    /// `iommu-map`: requester ids `rid_base..rid_base + len` reach the
    /// IOMMU `iommu` (a phandle) as stream `stream_base + (rid - rid_base)`.
    pub iommu_map: Vec<IommuMap>,
}

/// One `interrupt-map` entry, reduced to what a GIC parent says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Intx {
    /// The child unit address's first cell (bus, device, function) and pin.
    pub child: [u32; 4],
    /// The GIC interrupt id the pin raises, and whether it is a level.
    pub intid: u32,
    pub level: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IommuMap {
    pub rid_base: u32,
    pub iommu: u32,
    pub stream_base: u32,
    pub len: u32,
}

/// The node of the first enabled `pci-host-ecam-generic` bridge.
pub fn find(root: &Node) -> Option<&Node> {
    let mut found = None;
    walk(root, &mut |n| {
        if found.is_none()
            && n.prop_str_list("compatible")
                .is_ok_and(|c| c.contains("pci-host-ecam-generic"))
            && n.prop_str("status")
                .map_or(true, |s| s == "okay" || s == "ok")
        {
            found = Some(n as *const Node);
        }
    });
    // SAFETY: a node of the tree `root` borrows.
    found.map(|n| unsafe { &*n })
}

fn walk<'a>(n: &'a Node, f: &mut impl FnMut(&'a Node)) {
    f(n);
    for c in &n.children {
        walk(c, f);
    }
}

fn by_phandle(root: &Node, phandle: u32) -> Option<&Node> {
    let mut found = None;
    walk(root, &mut |n| {
        if found.is_none() && n.prop_u32("phandle").ok() == Some(phandle) {
            found = Some(n as *const Node);
        }
    });
    // SAFETY: as in `find`.
    found.map(|n| unsafe { &*n })
}

/// Read `host`, a bridge node whose parent bus has `parent_addr` address
/// and `parent_size` size cells (2 and 2 under QEMU `virt`'s root).
pub fn parse(root: &Node, host: &Node, parent_addr: u32, parent_size: u32) -> Option<PciHost> {
    let reg = host.prop_cells("reg").ok()?;
    let (a, s) = (parent_addr as usize, parent_size as usize);
    if reg.len() < a + s {
        return None;
    }
    let ecam = (cells_u64(&reg[..a])?, cells_u64(&reg[a..a + s])?);
    let buses = match host.prop_cells("bus-range").ok().as_deref() {
        Some([lo, hi]) => (*lo as u8, *hi as u8),
        _ => (0, 255),
    };
    let mask = match host.prop_cells("interrupt-map-mask").ok().as_deref() {
        Some(&[a, b, c, d]) => [a, b, c, d],
        _ => [u32::MAX; 4],
    };
    let mut intx = Vec::new();
    if let Ok(map) = host.prop_cells("interrupt-map") {
        let mut at = &map[..];
        while at.len() >= 5 {
            let child = [at[0], at[1], at[2], at[3]];
            let Some(parent) = by_phandle(root, at[4]) else {
                break;
            };
            let pa = parent.prop_u32("#address-cells").unwrap_or(0) as usize;
            let pi = parent.prop_u32("#interrupt-cells").unwrap_or(1) as usize;
            let Some(spec) = at.get(5 + pa..5 + pa + pi) else {
                break;
            };
            if let Some((intid, level)) = gic_intid(spec) {
                intx.push(Intx {
                    child,
                    intid,
                    level,
                });
            }
            at = &at[5 + pa + pi..];
        }
    }
    let iommu_map = host
        .prop_cells("iommu-map")
        .map(|m| {
            m.as_chunks::<4>()
                .0
                .iter()
                .map(|e| IommuMap {
                    rid_base: e[0],
                    iommu: e[1],
                    stream_base: e[2],
                    len: e[3],
                })
                .collect()
        })
        .unwrap_or_default();
    Some(PciHost {
        ecam,
        buses,
        mask,
        intx,
        iommu_map,
    })
}

fn cells_u64(c: &[u32]) -> Option<u64> {
    match c {
        [lo] => Some(*lo as u64),
        [hi, lo] => Some((*hi as u64) << 32 | *lo as u64),
        _ => None,
    }
}

/// A GIC specifier (type, number, flags) as its interrupt id: SPIs from 32,
/// PPIs from 16. Flags 4 and 8 are levels.
fn gic_intid(spec: &[u32]) -> Option<(u32, bool)> {
    let [kind, num, flags] = *spec else {
        return None;
    };
    let intid = match kind {
        0 => num + 32,
        1 => num + 16,
        _ => return None,
    };
    Some((intid, flags & 0xc != 0))
}

impl PciHost {
    /// The interrupt line `pin` (1 = INTA) of `bus:dev.func` raises, when the
    /// map names one. Only for a function on the root bus: one behind a
    /// bridge raises its pin swizzled through the bridges above, which this
    /// does not walk.
    pub fn intx_for(&self, bus: u8, dev: u8, func: u8, pin: u8) -> Option<&Intx> {
        if pin == 0 || pin > 4 || bus != self.buses.0 {
            return None;
        }
        let child = [
            (bus as u32) << 16 | (dev as u32) << 11 | (func as u32) << 8,
            0,
            0,
            pin as u32,
        ];
        self.intx
            .iter()
            .find(|e| (0..4).all(|i| e.child[i] & self.mask[i] == child[i] & self.mask[i]))
    }

    /// The IOMMU (phandle) and stream id the requester `rid` reaches.
    pub fn stream_for(&self, rid: u16) -> Option<(u32, u32)> {
        let rid = rid as u32;
        self.iommu_map
            .iter()
            .find(|m| (m.rid_base..m.rid_base + m.len).contains(&rid))
            .map(|m| (m.iommu, m.stream_base + rid - m.rid_base))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{string::String, vec};

    fn be(cells: &[u32]) -> Vec<u8> {
        cells.iter().flat_map(|c| c.to_be_bytes()).collect()
    }

    fn node(name: &str, props: &[(&str, Vec<u8>)], children: Vec<Node>) -> Node {
        Node {
            name: String::from(name),
            props: props
                .iter()
                .map(|(k, v)| (String::from(*k), v.clone()))
                .collect(),
            children,
        }
    }

    /// QEMU 8.2 `virt`, as `dtc` prints it: four slots' worth of the
    /// swizzle, SPIs 3 to 6, one SMMUv3 with phandle 0x8003.
    fn virt() -> Node {
        let gic = node(
            "intc@8000000",
            &[
                ("phandle", be(&[0x8002])),
                ("#address-cells", be(&[2])),
                ("#interrupt-cells", be(&[3])),
            ],
            vec![],
        );
        let mut map = Vec::new();
        for slot in 0..4u32 {
            for pin in 1..=4u32 {
                let spi = 3 + (slot + pin - 1) % 4;
                map.extend([slot << 11, 0, 0, pin, 0x8002, 0, 0, 0, spi, 4]);
            }
        }
        let pci = node(
            "pcie@10000000",
            &[
                ("compatible", b"pci-host-ecam-generic\0".to_vec()),
                ("reg", be(&[0x40, 0x1000_0000, 0, 0x1000_0000])),
                ("bus-range", be(&[0, 0xff])),
                ("interrupt-map-mask", be(&[0x1800, 0, 0, 7])),
                ("interrupt-map", be(&map)),
                ("iommu-map", be(&[0, 0x8003, 0, 0x10000])),
            ],
            vec![],
        );
        node("", &[], vec![gic, pci])
    }

    #[test]
    fn the_virt_bridge_gives_its_window_its_lines_and_its_streams() {
        let root = virt();
        let host = parse(&root, find(&root).unwrap(), 2, 2).unwrap();
        assert_eq!(host.ecam, (0x40_1000_0000, 0x1000_0000));
        assert_eq!(host.buses, (0, 0xff));
        assert_eq!(host.intx.len(), 16);
        // Slot 2, INTA: SPI 5, interrupt id 37, a level.
        let e = host.intx_for(0, 2, 0, 1).unwrap();
        assert_eq!((e.intid, e.level), (37, true));
        // Slot 6 is slot 2 again under the mask; function bits are masked.
        assert_eq!(host.intx_for(0, 6, 3, 1).unwrap().intid, 37);
        assert_eq!(host.intx_for(0, 2, 0, 2).unwrap().intid, 38);
        assert!(host.intx_for(0, 2, 0, 0).is_none());
        assert!(host.intx_for(1, 0, 0, 1).is_none());
        assert_eq!(host.stream_for(0x0010), Some((0x8003, 0x10)));
    }

    #[test]
    fn a_tree_with_no_bridge_has_none() {
        let root = node("", &[], vec![]);
        assert!(find(&root).is_none());
    }
}
