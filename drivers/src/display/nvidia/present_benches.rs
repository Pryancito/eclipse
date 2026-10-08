use super::surfaceflip_tests::{console_gpu, FB, H_MEMORY, SERIAL};
use super::*;
use test::{black_box, Bencher};

/// Register `extra` further framebuffers beside the one `console_gpu`
/// already has, the way a compositor's swapchain does.
fn with_swapchain(gpu: &NvidiaGpu, extra: u32) {
    let mut fbs = gpu.kms_framebuffers.lock();
    for i in 0..extra {
        fbs.push(NvidiaKmsFramebuffer {
            id: FB + 1 + i,
            handle_id: 2 + i,
            width: 1920,
            height: 1080,
            pitch: 1920 * 4,
            phys_addr: 0,
            size: 0,
            h_memory: H_MEMORY,
            vram_offset: Some(0),
        });
    }
}

/// A flip on a console GPU with the ladder already up: the steady state,
/// once per frame for as long as the desktop is running. The first flip is
/// taken outside the measurement so the lazy `hwflip_init` is not counted
/// here — it has its own row.
#[bench]
fn flip_a_single_framebuffer(b: &mut Bencher) {
    let _g = SERIAL.lock();
    let gpu = console_gpu();
    assert!(gpu.page_flip(FB), "the ladder comes up on the first flip");
    assert!(gpu.has_hardware_kms());
    b.iter(|| black_box(gpu.page_flip(black_box(FB))));
}

// There is deliberately no row here for bringing the flip ladder up.
// Measuring it needs a fresh GPU per iteration, and `console_gpu` costs
// hundreds of microseconds to build with a spread wider than its own mean,
// so the build it is meant to isolate sits well under the fixture's noise:
// the two rows that tried came out with a variance sixteen times the mean
// and an "empty equivalent" dearer than the row it was bounding, which
// measures the fixture and nothing else. What justifies the lazy build and
// its latch is the shape — once per modeset, against a flip that is the
// row above, per frame — not a figure this harness can produce.

/// Flipping round a two-buffer swapchain, which is what wlroots runs.
/// `page_flip` looks its framebuffer up by id in a `Vec` under a lock, so
/// this and the two rows below say whether that lookup is flat in the
/// swapchain's depth or walks it.
#[bench]
fn flip_round_a_swapchain_of_2(b: &mut Bencher) {
    bench_swapchain(b, 2);
}

/// Four buffers, which a compositor with a cursor plane or a
/// triple-buffered output reaches.
#[bench]
fn flip_round_a_swapchain_of_4(b: &mut Bencher) {
    bench_swapchain(b, 4);
}

/// Sixteen, past anything real: here a per-flip walk would be unmissable.
/// The three rows come out level, which is the answer — but their spread
/// is wide enough that what they rule out is a walk, not a few
/// nanoseconds of drift with depth.
#[bench]
fn flip_round_a_swapchain_of_16(b: &mut Bencher) {
    bench_swapchain(b, 16);
}

fn bench_swapchain(b: &mut Bencher, depth: u32) {
    let _g = SERIAL.lock();
    let gpu = console_gpu();
    with_swapchain(&gpu, depth - 1);
    // Each buffer's ctxdma is built on its first flip; the measurement is
    // the steady state after all of them are built.
    for i in 0..depth {
        assert!(gpu.page_flip(FB + i), "buffer {} flips", i);
    }
    let mut next = 0u32;
    b.iter(|| {
        let id = FB + next;
        next = (next + 1) % depth;
        black_box(gpu.page_flip(black_box(id)))
    });
}

/// A flip naming a framebuffer that was never registered: the lookup
/// misses and the call returns at once. The floor for a row of this
/// shape — a flip that does no work — so the flip rows above can be read
/// against something that only pays the lookup.
#[bench]
fn refuse_a_flip_of_an_unknown_framebuffer(b: &mut Bencher) {
    let _g = SERIAL.lock();
    let gpu = console_gpu();
    assert!(gpu.page_flip(FB));
    b.iter(|| black_box(gpu.page_flip(black_box(FB + 999))));
}

/// `wait_vblank`, which a compositor calls once per frame to pace itself.
///
/// Under this fixture the test clock does not advance, so the call finds
/// the stamp the flip left and returns at once: the row lands on
/// `b.iter`'s own floor and says nothing about how long a real wait takes.
/// What it does say is the regression it guards — that the call does not
/// spin a frame on the calling CPU — because a path that spun could not
/// land there however the clock behaved.
#[bench]
fn wait_for_a_vblank(b: &mut Bencher) {
    let _g = SERIAL.lock();
    let gpu = console_gpu();
    assert!(gpu.page_flip(FB), "a flip stamps last_vblank_us");
    b.iter(|| black_box(gpu.wait_vblank(black_box(0))));
}

/// The stats line `/proc` serves: eight atomics read and a `String`
/// formatted. Read by a human, not per frame — the row is here to say
/// whether anything in the flip path would want to avoid calling it.
#[bench]
fn format_the_surfaceflip_stats_line(b: &mut Bencher) {
    let _g = SERIAL.lock();
    let gpu = console_gpu();
    assert!(gpu.page_flip(FB));
    b.iter(|| black_box(surfaceflip_stats_line()));
}

// --- the probe decodes, once per GPU rather than per frame ---

/// `identify_gpu` on this hardware's own device id: a `match` over every
/// NVIDIA part the table names.
#[bench]
fn identify_the_rtx_2060_super(b: &mut Bencher) {
    b.iter(|| black_box(identify_gpu(black_box(0x1F06))));
}

/// A device id in no arm of the table: the fallthrough. If the `match`
/// compiles to a jump table this lands with the row above; if it is a
/// comparison chain, the unknown part walks the whole table.
#[bench]
fn identify_an_unknown_gpu(b: &mut Bencher) {
    b.iter(|| black_box(identify_gpu(black_box(0x0001))));
}

/// The empty equivalent of the two rows above: a call the compiler will
/// not inline returning the same three-field tuple and deciding nothing.
/// Without it, two identification rows landing together says only that
/// they cost the same, not that the `match` is free.
#[bench]
fn the_identification_floor(b: &mut Bencher) {
    #[inline(never)]
    fn decides_nothing(id: u16) -> (NvidiaArchitecture, &'static str, u32) {
        if id == u16::MAX {
            (NvidiaArchitecture::Unknown, "never", 0)
        } else {
            (NvidiaArchitecture::Turing, "always", 8192)
        }
    }
    b.iter(|| black_box(decides_nothing(black_box(0x1F06))));
}

/// `arch_from_pmc_boot0`: a shift, a mask and a chain of range tests, run
/// on the word read out of BAR0 when the device id is not enough.
#[bench]
fn decode_a_turing_boot0(b: &mut Bencher) {
    let word = black_box(0x166u32 << regs::PMC_BOOT0_CHIP_ID_SHIFT);
    b.iter(|| black_box(arch_from_pmc_boot0(black_box(word))));
}

/// The same decode for a chip id past every range: the last arm of the
/// chain, which is the longest path through it.
#[bench]
fn decode_an_unknown_boot0(b: &mut Bencher) {
    let word = black_box(0x001u32 << regs::PMC_BOOT0_CHIP_ID_SHIFT);
    b.iter(|| black_box(arch_from_pmc_boot0(black_box(word))));
}

/// The fault decode, which runs per GPU page fault while a fault storm is
/// being logged — the one of these three that is not once per probe.
#[bench]
fn name_a_fault_reason_and_access(b: &mut Bencher) {
    b.iter(|| {
        let info1 = black_box(0x0001_0002u32);
        black_box((
            fault_reason_name(info1 & 0x1f),
            fault_access_name((info1 >> 16) & 0xf),
        ))
    });
}
