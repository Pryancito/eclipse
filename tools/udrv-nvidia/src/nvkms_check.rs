//! `udrv-nvidia nvkms-check`: the way nvidia-modeset is asked for on a
//! card whose driver is this process, checked with no GPU.
//!
//! `nvidia.nvkms` has the kernel give each of its NVIDIA cards to
//! nvidia-modeset once the console GPU is up. The cards of the GPUs left to
//! this driver (`udriver=nvidia@N`) are no card of the kernel's. So on a
//! boot that brings the console GPU up (`nvidia.console_gpu`) it asks the
//! driver, over the card that scans out the console
//! (`drm_remote::op::NVKMS_LOAD`), and then what the heads say
//! (`SCANOUT_LIVENESS`): on that GPU the load is the modeset that puts the
//! console back on the monitor after the bring-up. The driver's other
//! cards are not asked, and on a boot that leaves the console GPU cold none
//! is: that boot stays as it was before this driver could load
//! nvidia-modeset at all.
//!
//! In QEMU (a machine with no NVIDIA GPU leaves none, and the kernel still
//! waits for the card of the driver its line asks for):
//!
//! - On a boot with `udriver=nvidia@0:nvidia.nvkms:nvidia.console_gpu`,
//!   `udrv-nvidia nvkms-check`: serve two cards with nothing behind them,
//!   each at a place on the bus (`drm::check::Card`), one of which says it
//!   scans out the console. The kernel asks that one for nvidia-modeset,
//!   once, after the console's bring-up, logs what it answers, and asks for
//!   its heads. The other is never asked. Meanwhile `/proc/gpuready` holds
//!   the session (init starts it on "ready") from before the bring-up until
//!   nvidia-modeset has been asked for: a session that started earlier was
//!   running when the window opened, which the window cannot have.
//! - `udrv-nvidia nvkms-check again`, after it: a driver that started
//!   again serves the same GPU, and is not asked a second time in the boot;
//!   its console GPU, cold now, holds no session back.
//! - On a boot with `udriver=nvidia@0:nvidia.nvkms` and no
//!   `nvidia.console_gpu`, `udrv-nvidia nvkms-check never`: the card that
//!   says it scans out the console is not asked at all, and holds no
//!   session back.
use std::process::exit;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::drm::{self, check::Card};
use crate::{fail, NAME};

/// Where the cards say they are: places no PCI function has.
const CONSOLE: (u32, u8, u8, u8) = (0, 0xfe, 0x1f, 0);
const COMPUTE: (u32, u8, u8, u8) = (0, 0xfe, 0x1e, 0);

/// The console's bring-up comes first: it waits for a compositor up to 45 s
/// from boot, or 4 s from the first read of `/proc/gpuready`, which this
/// makes. The kernel's task then looks for cards to ask twice a second.
const ASKED_WITHIN: Duration = Duration::from_secs(60);
const NOT_ASKED_FOR: Duration = Duration::from_secs(5);

/// Serve `card` on a thread of its own, for as long as the process lives.
fn serve(card: &Arc<Card>) {
    let served = card.clone();
    std::thread::spawn(move || match drm::serve(served) {
        Ok(why) => fail(format!("the kernel let go of the card: {why}")),
        Err(e) => fail(e),
    });
}

pub fn nvkms_check(arg: Option<&str>) {
    let (again, never) = match arg {
        None => (false, false),
        Some("again") => (true, false),
        Some("never") => (false, true),
        Some(_) => fail("usage: udrv-nvidia nvkms-check [again|never]".into()),
    };
    let ok = |what: &str, ok: bool| {
        println!("{NAME}: {what}: {}", if ok { "ok" } else { "FAILED" });
        if !ok {
            exit(1);
        }
    };
    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    if !(cmdline.contains("nvidia.nvkms") && cmdline.contains("udriver=nvidia@")) {
        fail("the kernel's line needs udriver=nvidia@0 and nvidia.nvkms".into());
    }
    if cmdline.contains("nvidia.console_gpu") == never {
        fail(if never {
            "nvkms-check never is for a boot without nvidia.console_gpu".into()
        } else {
            "the kernel's line needs nvidia.console_gpu: only the console GPU it brings up is asked"
                .into()
        });
    }
    let console = Arc::new(Card {
        place: Some(CONSOLE),
        console: true,
        ..Default::default()
    });
    let compute = Arc::new(Card {
        place: Some(COMPUTE),
        ..Default::default()
    });
    serve(&console);
    if !again {
        serve(&compute);
    }
    // What init's wait for the GPU reads, and what starts a console
    // bring-up that is waiting for a compositor.
    let gpuready = || std::fs::read_to_string("/proc/gpuready").unwrap_or_default();
    let _ = gpuready();
    let waiting = |report: &str| report.starts_with("gpuready: waiting");
    let loads = |card: &Card| card.nvkms_loads.load(Ordering::Relaxed);
    let after_nvkms = || {
        let phases = console.liveness.lock().unwrap();
        phases.iter().filter(|p| *p == "post-nvkms").count()
    };
    let until = |d: Duration, done: &dyn Fn() -> bool| {
        let t = Instant::now();
        while !done() && t.elapsed() < d {
            std::thread::sleep(Duration::from_millis(50));
        }
        done()
    };
    if again {
        until(NOT_ASKED_FOR, &|| loads(&console) != 0);
        ok(
            "a driver that started again is not asked for nvidia-modeset twice in a boot",
            loads(&console) == 0 && after_nvkms() == 0,
        );
        // Its console GPU is cold and no bring-up is coming: it holds no
        // session, which starts on what there is.
        ok(
            "...and its cold console GPU holds no session back",
            !waiting(&gpuready()),
        );
    } else if never {
        until(NOT_ASKED_FOR, &|| loads(&console) != 0);
        ok(
            "on a boot that leaves the console GPU cold no card is asked for nvidia-modeset",
            loads(&console) == 0 && loads(&compute) == 0 && after_nvkms() == 0,
        );
        ok(
            "...and that GPU holds no session back",
            !waiting(&gpuready()),
        );
    } else {
        // The session is init's to start when /proc/gpuready says ready:
        // not while the console GPU of this driver is still to be brought
        // up, nor before nvidia-modeset has it.
        let held =
            |report: &str| waiting(report) && report.lines().any(|l| l.contains("in udrv-nvidia"));
        ok(
            "the session is held back for the console GPU this driver is to bring up",
            until(NOT_ASKED_FOR, &|| held(&gpuready())),
        );
        ok(
            "...which the kernel asks it to, and for its GR ladder",
            until(ASKED_WITHIN, &|| {
                console.brought_up.load(Ordering::Relaxed) && console.ladder.load(Ordering::Relaxed)
            }),
        );
        ok(
            "...and it is still held then, until nvidia-modeset has been asked for",
            held(&gpuready()) || loads(&console) == 1,
        );
        ok(
            "the kernel asks the console card's driver to load nvidia-modeset",
            until(ASKED_WITHIN, &|| loads(&console) == 1),
        );
        ok(
            "...and the session may start: /proc/gpuready says ready",
            until(NOT_ASKED_FOR, &|| gpuready().starts_with("gpuready: ready")),
        );
        print!("{}", gpuready());
        ok(
            "...and then what its heads say",
            until(ASKED_WITHIN, &|| after_nvkms() == 1),
        );
        until(NOT_ASKED_FOR, &|| loads(&console) != 1);
        ok("...once", loads(&console) == 1 && after_nvkms() == 1);
        ok(
            "...and not the card that does not scan out the console",
            loads(&compute) == 0,
        );
    }
    println!("{NAME}: nvkms-check: ok");
}
