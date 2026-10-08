use super::*;

/// A NV_PMC_BOOT_0 word carrying `chip_id` in its 9-bit field.
fn boot0(chip_id: u32) -> u32 {
    chip_id << regs::PMC_BOOT0_CHIP_ID_SHIFT
}

/// The box this is developed against: two RTX 2060 Super. Both of the
/// device ids that part ships under must land on Turing with its real
/// 8 GiB, or `nouveau_engine_classes` refuses and the client loses every
/// Vulkan GPU (see `nouveau_arch`).
#[test]
fn the_rtx_2060_super_is_recognised_under_every_device_id_it_ships_as() {
    for id in [0x1F02u16, 0x1F06, 0x1F07] {
        let (arch, name, vram) = identify_gpu(id);
        assert_eq!(arch, NvidiaArchitecture::Turing, "device id {:#06x}", id);
        assert_eq!(name, "GeForce RTX 2060 Super");
        assert_eq!(vram, 8192, "8 GiB, in MiB");
    }
    // The plain 2060 shares the 0x1F0x block and must NOT be conflated
    // with the Super: different VRAM size.
    assert_eq!(identify_gpu(0x1F03).2, 6144);
}

/// `Unknown` is the table's "I have no idea" arm, and a VRAM size of 0 is
/// what the probe treats as "ask the hardware". They must agree: a named
/// part with 0 MiB, or an unknown part claiming a size, would both be
/// read as facts downstream.
#[test]
fn a_table_entry_is_either_fully_known_or_fully_unknown() {
    for id in 0x0000u16..=0xFFFF {
        let (arch, name, vram) = identify_gpu(id);
        if arch == NvidiaArchitecture::Unknown {
            assert_eq!(vram, 0, "unknown part {:#06x} claims {} MiB", id, vram);
            assert_eq!(name, "Unknown NVIDIA GPU", "device id {:#06x}", id);
        } else {
            assert!(vram > 0, "{} ({:#06x}) reports no VRAM", name, id);
            assert_ne!(name, "Unknown NVIDIA GPU", "device id {:#06x}", id);
        }
    }
}

/// The RTX 2060 Super is a TU106, chip id 0x166 — dead centre of the
/// Turing range, and the only decode that matters on this hardware.
#[test]
fn a_turing_boot0_decodes_to_turing() {
    assert_eq!(
        arch_from_pmc_boot0(boot0(0x166)),
        NvidiaArchitecture::Turing
    );
    // The revision nibble and every other low bit are not part of the id.
    assert_eq!(
        arch_from_pmc_boot0(boot0(0x166) | 0x000f_ffff),
        NvidiaArchitecture::Turing
    );
}

/// The regression this guards. `nouveau_chipset_id` carries, per
/// architecture, the chip id it reports when NV_PMC_BOOT_0 is unreadable,
/// each annotated with the real part (0x162 TU102 ... 0x1b2 GB202). Those
/// ids and the range table are two statements about the same numbering, so
/// feeding each id back through the decoder has to return the
/// architecture it stands for. It did not: Hopper's range was 0x1B0..=0x1BF
/// — which is where consumer Blackwell actually lives — so a GB202 decoded
/// as Hopper and got Hopper's DMA-copy classes instead of `BLACKWELL_B`,
/// while GH100's own 0x180 fell in the gap and decoded as Unknown.
#[test]
fn every_representative_chip_id_decodes_to_its_own_architecture() {
    for (chip_id, arch) in [
        (0x162u32, NvidiaArchitecture::Turing),   // TU102
        (0x166, NvidiaArchitecture::Turing),      // TU106
        (0x172, NvidiaArchitecture::Ampere),      // GA102
        (0x192, NvidiaArchitecture::AdaLovelace), // AD102
        (0x180, NvidiaArchitecture::Hopper),      // GH100
        (0x1b2, NvidiaArchitecture::Blackwell),   // GB202
    ] {
        assert_eq!(
            arch_from_pmc_boot0(boot0(chip_id)),
            arch,
            "chip id {:#x}",
            chip_id
        );
    }
}

/// Why the old `BLACKWELL_MIN = 0x200` could never fire: nouveau reads the
/// chip id as `(boot0 & 0x1ff00000) >> 20`, and `nouveau_chipset_id` does
/// the same, so the largest id any part can report is 0x1FF. A lower bound
/// of 0x200 made the Blackwell arm dead code. Assert the bound stays
/// inside the field the hardware actually has.
#[test]
fn no_architecture_bound_sits_outside_the_nine_bit_chip_id_field() {
    const CHIP_ID_MAX: u32 = 0x1FF; // 9 bits, as nouveau reads it
    for bound in [
        regs::PMC_BOOT0_CHIPID_TURING_MIN,
        regs::PMC_BOOT0_CHIPID_TURING_MAX,
        regs::PMC_BOOT0_CHIPID_AMPERE_MIN,
        regs::PMC_BOOT0_CHIPID_AMPERE_MAX,
        regs::PMC_BOOT0_CHIPID_ADA_MIN,
        regs::PMC_BOOT0_CHIPID_ADA_MAX,
        regs::PMC_BOOT0_CHIPID_HOPPER_MIN,
        regs::PMC_BOOT0_CHIPID_HOPPER_MAX,
        regs::PMC_BOOT0_CHIPID_BLACKWELL_MIN,
    ] {
        assert!(bound <= CHIP_ID_MAX, "bound {:#x} is unreachable", bound);
    }
}

/// The ranges must not overlap, or the `else if` chain silently decides by
/// source order rather than by the numbering.
#[test]
fn the_architecture_ranges_do_not_overlap() {
    let mut seen: alloc::vec::Vec<(u32, NvidiaArchitecture)> = alloc::vec::Vec::new();
    for chip_id in 0..=0x1FFu32 {
        let arch = arch_from_pmc_boot0(boot0(chip_id));
        if arch != NvidiaArchitecture::Unknown {
            seen.push((chip_id, arch));
        }
    }
    // Each known id belongs to exactly one architecture, and the
    // architectures form contiguous blocks in increasing order.
    let mut blocks: alloc::vec::Vec<NvidiaArchitecture> = alloc::vec::Vec::new();
    for (_, arch) in &seen {
        if blocks.last() != Some(arch) {
            assert!(
                !blocks.contains(arch),
                "{:?} appears in two separate blocks",
                arch
            );
            blocks.push(*arch);
        }
    }
    assert_eq!(
        blocks,
        alloc::vec![
            NvidiaArchitecture::Turing,
            NvidiaArchitecture::Ampere,
            NvidiaArchitecture::Hopper,
            NvidiaArchitecture::AdaLovelace,
            NvidiaArchitecture::Blackwell,
        ]
    );
}

/// `user_slice_ok` is the `access_ok()` on the arrays a nouveau ioctl
/// dereferences directly (`op_ptr`, `push_ptr`, `wait_ptr`, `sig_ptr`).
/// The render node is 0666, so anything it lets through is a read at an
/// address an unprivileged caller chose.
#[test]
fn user_slice_ok_rejects_what_a_render_node_caller_must_not_reach() {
    // A plain user array is fine.
    assert!(user_slice_ok::<u64>(0x1000, 8));
    // Zero elements need no pointer at all — the ioctl arms skip the read.
    assert!(user_slice_ok::<u64>(0, 0));
    // ... but a non-empty array at NULL is not a slice.
    assert!(!user_slice_ok::<u64>(0, 1));
    // A kernel-half address: the kernel is mapped in every address space,
    // so this used to resolve and turn the copy into an arbitrary read.
    assert!(!user_slice_ok::<u64>(0xffff_8000_0000_0000, 1));
    // The canonical split itself is the first address that is not user.
    const USER_MAX: u64 = 0x0000_8000_0000_0000;
    assert!(user_slice_ok::<u8>(USER_MAX - 1, 1));
    assert!(!user_slice_ok::<u8>(USER_MAX, 1));
    // A range that starts low and ENDS above the split must be refused as
    // a whole, not clipped.
    assert!(!user_slice_ok::<u8>(USER_MAX - 4, 8));
    // A huge element count must fail closed rather than wrap: the byte
    // length is `count * size_of::<T>()`, and `checked_mul` catches the
    // cases a 32-bit host would wrap on while the range check catches the
    // rest. Either way the answer has to be "no".
    assert!(!user_slice_ok::<[u8; 0x1_0000]>(0x1000, u32::MAX));
}

/// The regression this guards. `page_flip`'s copy-engine path passed
/// `fb.width * 4` and `fb.height` straight to the CE without ever comparing
/// them against the mode, unlike `scanout_region`, which has always clipped
/// with `.min(info.width)` / `.min(info.height)`.
#[test]
fn a_hardware_flip_is_clipped_to_the_mode_it_is_flipping_into() {
    // A client framebuffer wider and taller than the mode: clipped to the
    // mode on both axes, not passed through.
    assert_eq!(
        hwflip_geometry(2560, 1440, 2560 * 4, 1920, 1080, 1920 * 4),
        Some((1920 * 4, 1080))
    );
    // The ordinary case is untouched: fb exactly the mode.
    assert_eq!(
        hwflip_geometry(1920, 1080, 1920 * 4, 1920, 1080, 1920 * 4),
        Some((1920 * 4, 1080))
    );
    // A padded destination pitch is fine -- the row is narrower than the
    // stride, which is exactly what a pitched 2D copy is for. This is the
    // documented dual-RTX case: client pitch 5504, GOP pitch 8192.
    assert_eq!(
        hwflip_geometry(1376, 1080, 5504, 1376, 1080, 8192),
        Some((1376 * 4, 1080))
    );
    // A framebuffer SMALLER than the mode copies only what it has; the rest
    // of the screen is not this flip's business.
    assert_eq!(
        hwflip_geometry(800, 600, 800 * 4, 1920, 1080, 1920 * 4),
        Some((800 * 4, 600))
    );
}

/// Refusing is the safe answer, and it matters more than it looks: a row
/// wider than the destination pitch makes the RM reject the copy with
/// `NV_ERR_INVALID_ARGUMENT`, and `CE_PRESENT_WEDGED` then latches for the
/// whole boot -- every later present silently degrades to the CPU blit. A
/// `None` here costs one frame's fast path instead.
#[test]
fn a_flip_that_cannot_be_expressed_as_a_pitched_copy_is_refused() {
    // Row wider than the destination stride: would shear each row into the
    // next inside the scanout framebuffer.
    assert_eq!(
        hwflip_geometry(1920, 1080, 1920 * 4, 1920, 1080, 1024),
        None
    );
    // Row wider than the SOURCE stride: would read the next row's pixels as
    // this row's tail.
    assert_eq!(
        hwflip_geometry(1920, 1080, 1024, 1920, 1080, 1920 * 4),
        None
    );
    // Degenerate geometry on either side.
    assert_eq!(
        hwflip_geometry(0, 1080, 1920 * 4, 1920, 1080, 1920 * 4),
        None
    );
    assert_eq!(
        hwflip_geometry(1920, 0, 1920 * 4, 1920, 1080, 1920 * 4),
        None
    );
    assert_eq!(hwflip_geometry(1920, 1080, 0, 1920, 1080, 1920 * 4), None);
    assert_eq!(hwflip_geometry(1920, 1080, 1920 * 4, 1920, 1080, 0), None);
    assert_eq!(
        hwflip_geometry(1920, 1080, 1920 * 4, 0, 1080, 1920 * 4),
        None
    );
    assert_eq!(
        hwflip_geometry(1920, 1080, 1920 * 4, 1920, 0, 1920 * 4),
        None
    );
    // A width whose byte count would overflow must not wrap into a small
    // row that then passes the stride checks.
    assert_eq!(
        hwflip_geometry(u32::MAX, 1, u32::MAX, u32::MAX, 1, u32::MAX),
        None
    );
}

/// `elapsed: N ns` is how `/proc/gpubench` reports a launch, and 0 is the
/// "no measurement" answer the caller expects for anything unparseable.
#[test]
fn gpubench_elapsed_is_parsed_or_reported_as_zero() {
    assert_eq!(parse_gpubench_elapsed_ns("elapsed: 1234 ns"), 1234);
    // The interesting line is not the first one.
    assert_eq!(
        parse_gpubench_elapsed_ns("saxpy ok\ngrid: 2176 threads\nelapsed: 98765 ns\n"),
        98765
    );
    // No such field, an empty report, and a field with no digits all mean
    // "nothing measured" rather than a made-up number.
    assert_eq!(parse_gpubench_elapsed_ns("saxpy ok\n"), 0);
    assert_eq!(parse_gpubench_elapsed_ns(""), 0);
    assert_eq!(parse_gpubench_elapsed_ns("elapsed: unknown"), 0);
    // The unit suffix is not part of the number.
    assert_eq!(parse_gpubench_elapsed_ns("elapsed: 42ns"), 42);
}

/// Connector types cross into DRM's own numbering, which userspace reads
/// back from GETCONNECTOR. Two things must hold: an unmapped NVIDIA type
/// comes out as DRM `Unknown` (0) rather than as whatever NVIDIA's number
/// happens to be, and the numeric table and the name table agree on which
/// types they know -- they are maintained by hand, side by side, and a
/// connector named in one but not the other is a display Eclipse either
/// mislabels in `/proc/gpuedid` or hands userspace as type 0.
#[test]
fn the_connector_type_tables_agree_on_what_they_know() {
    assert_eq!(nv_conn_type_to_drm(0xDEAD_BEEF), 0);
    assert_eq!(nv_conn_type_name(0xDEAD_BEEF), "other");

    for t in 0u32..=0xFFFF {
        let drm = nv_conn_type_to_drm(t);
        let named = nv_conn_type_name(t) != "other";
        assert_eq!(
            drm != 0,
            named,
            "NVIDIA connector type {:#x}: drm={} name={:?}",
            t,
            drm,
            nv_conn_type_name(t)
        );
    }
    // Moebius's monitors hang off DisplayPort and HDMI; those two are the
    // ones that must not regress.
    assert_eq!(
        nv_conn_type_to_drm(0x46),
        10,
        "DRM_MODE_CONNECTOR_DisplayPort"
    );
    assert_eq!(nv_conn_type_to_drm(0x61), 11, "DRM_MODE_CONNECTOR_HDMIA");
}

/// The ELD the HDMI audio path builds when the bootloader kept only the
/// 128-byte base EDID. The baseline length in byte 2 is counted in
/// dwords from byte 4, so it has to cover every byte actually written —
/// the monitor name and the SAD included — or the codec stops reading
/// before the sample rates.
#[test]
fn the_eld_baseline_length_covers_the_bytes_written_into_it() {
    // An EDID with no monitor-name descriptor: MNL 0, SAD at byte 20.
    let mut edid = [0u8; 128];
    edid[8..12].copy_from_slice(&[0x04, 0x21, 0x37, 0x13]); // manufacturer + product
    let eld = build_eld_from_base_edid(&edid, 0x1234_5678, false);
    assert_eq!(eld[4], 0, "no name descriptor, so MNL is 0");
    let baseline_end = 4 + eld[2] as usize * 4;
    assert!(
        baseline_end >= 20 + 3,
        "baseline ({} bytes) cuts off the SAD at 20..23",
        baseline_end - 4
    );
    assert_eq!(&eld[20..23], &[0x09, 0x07, 0x07], "2ch LPCM SAD");
    assert_eq!(eld[5] >> 4, 1, "exactly one SAD");
    assert_eq!(eld[5] & (1 << 2), 0, "HDMI, not DisplayPort");
    // The EDID vendor/product bytes are forwarded verbatim.
    assert_eq!(&eld[16..20], &edid[8..12]);
    // The display id is stored little-endian across bytes 8..12.
    assert_eq!(
        u32::from_le_bytes([eld[8], eld[9], eld[10], eld[11]]),
        0x1234_5678
    );

    // Now with a 0xFC monitor-name descriptor: MNL 13, so the SAD moves to
    // byte 33 and the baseline length has to grow with it.
    let mut named = [0u8; 128];
    named[54..58].copy_from_slice(&[0, 0, 0, 0xFC]);
    named[59..72].copy_from_slice(b"Eclipse Disp\0");
    let eld = build_eld_from_base_edid(&named, 0, true);
    assert_eq!(eld[4], 13, "a 0xFC descriptor gives a 13-byte name");
    assert_eq!(&eld[20..33], b"Eclipse Disp\0");
    assert_eq!(&eld[33..36], &[0x09, 0x07, 0x07], "the SAD moved past it");
    let baseline_end = 4 + eld[2] as usize * 4;
    assert!(
        baseline_end >= 36,
        "baseline ({} bytes) cuts off the SAD at 33..36",
        baseline_end - 4
    );
    assert_ne!(eld[5] & (1 << 2), 0, "DisplayPort was requested");

    // A short EDID is not parsed at all: an all-zero ELD, never a read
    // past the end of the buffer.
    assert_eq!(build_eld_from_base_edid(&[0u8; 127], 0, false), [0u8; 96]);
}
