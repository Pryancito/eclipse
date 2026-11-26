//! `udrv-nvidia window-check VVVV DDDD [unasked]`: the kernel's own window
//! onto device memory the boot mapped none of, on any PCI function with a
//! memory BAR past the machine's RAM (`udriver=pci-VVVV-DDDD@0`). On QEMU:
//! `ivshmem-plain` (1af4:1110), whose BAR 2 is 64-bit, which the firmware
//! places above the RAM.
//!
//! The kernel reads a buffer a GPU's driver placed in VRAM through its own
//! alias of the address, and the boot mapped that alias as far as RAM and
//! the boot framebuffer go. `check` asks for a window onto a BAR below
//! 4 GiB, and the boot maps the first 4 GiB whole on any machine: it shows
//! the way to the window, and not that anything is mapped. Here the BAR is
//! past all the boot mapped, as a GPU's VRAM is on a PC:
//!
//! - the kernel maps its window when the driver asks for it
//!   (`gem_mmap::window`), or, with `unasked`, when the driver registers an
//!   object there without having asked;
//! - it then reads the object where it is, for the pointer's image: in
//!   this process, and in one that was running before the window was
//!   mapped. An address space has a copy of the top-level entries of the
//!   kernel's tables from when it was made, so the second is what the bus
//!   scan's `user::window::reach` is for, when the BAR is above 512 GiB,
//!   below an entry RAM is not below.
//!
//! Against a kernel from before the window, `unasked` is the fault this is
//! about: the kernel reads through an alias that nothing mapped.
use std::process::exit;

use zcore_drivers::builder::IoMapper;
use zcore_drivers::scheme::gem_mmap::{self, HandleSlice};

use crate::{fail, gpu, pointer_image_from, NAME};

const GIB: usize = 1 << 30;

/// A pointer's image: 64x64 pixels of four bytes.
const IMAGE: usize = 64 * 64 * 4;

/// A process forked off this one, which waits for a GEM handle and has the
/// kernel read that object for the pointer's image.
struct Earlier {
    pid: libc::pid_t,
    ask: libc::c_int,
    answer: libc::c_int,
}

impl Earlier {
    /// Fork it now: its address space is as the kernel's tables are now.
    fn fork() -> Earlier {
        let (mut ask, mut answer) = ([0; 2], [0; 2]);
        // SAFETY: room for the two descriptors of each pipe.
        if unsafe { libc::pipe(ask.as_mut_ptr()) != 0 || libc::pipe(answer.as_mut_ptr()) != 0 } {
            fail(format!("pipe: {}", std::io::Error::last_os_error()));
        }
        // SAFETY: this process has one thread still, and holds no lock.
        match unsafe { libc::fork() } {
            -1 => fail(format!("fork: {}", std::io::Error::last_os_error())),
            0 => {
                let mut handle = [0u8; 4];
                // SAFETY: its own ends of the pipes, and four bytes to read
                // into.
                let asked = unsafe {
                    libc::close(ask[1]);
                    libc::close(answer[0]);
                    libc::read(ask[0], handle.as_mut_ptr().cast(), 4) == 4
                };
                let said = match asked.then(|| pointer_image_from(u32::from_ne_bytes(handle))) {
                    Some(Some(true)) => b'1',
                    Some(Some(false)) => b'0',
                    Some(None) => b'-',
                    None => b'?',
                };
                // SAFETY: one byte to write, and nothing of the parent's is
                // run on the way out.
                unsafe {
                    libc::write(answer[1], [said].as_ptr().cast(), 1);
                    libc::_exit(0)
                }
            }
            pid => {
                // SAFETY: the child's ends, which this process does not use.
                unsafe {
                    libc::close(ask[0]);
                    libc::close(answer[1]);
                }
                Earlier {
                    pid,
                    ask: ask[1],
                    answer: answer[0],
                }
            }
        }
    }

    /// Have the kernel read the object of `handle` in that process: whether
    /// it did, or what became of the process instead.
    fn read(self, handle: u32) -> Result<bool, String> {
        let (bytes, mut said, mut status) = (handle.to_ne_bytes(), [0u8; 1], 0);
        // SAFETY: four bytes to write and one to read into, the pipes' ends
        // of this process, and the child forked above.
        let answered = unsafe {
            libc::write(self.ask, bytes.as_ptr().cast(), 4);
            let n = libc::read(self.answer, said.as_mut_ptr().cast(), 1);
            libc::waitpid(self.pid, &mut status, 0);
            n == 1
        };
        match (answered, said[0]) {
            (true, b'1') => Ok(true),
            (true, b'0') => Ok(false),
            (true, b'-') => Err("no node of /dev/dri has a CRTC".into()),
            _ => Err(format!("it ended with no answer (wait status {status:#x})")),
        }
    }
}

/// Where the boot's map of memory ends at the latest: the machine's RAM,
/// rounded up to a GiB for what the kernel does not count, above the 4 GiB
/// that hold the hole the firmware leaves in it.
fn end_of_ram() -> Option<usize> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let total = meminfo.lines().find_map(|l| l.strip_prefix("MemTotal:"))?;
    let kib: usize = total.trim().trim_end_matches("kB").trim().parse().ok()?;
    Some((kib * 1024).div_ceil(GIB) * GIB + 4 * GIB)
}

pub fn window_check(vendor: u16, device: u16, asked: bool) {
    let ok = |what: &str, ok: bool| {
        println!("{NAME}: {what}: {}", if ok { "ok" } else { "FAILED" });
        if !ok {
            exit(1);
        }
    };
    // Before this process holds anything of the device.
    let earlier = Earlier::fork();
    let gpus = gpu::claim_all(|index| gpu::Which::Pci {
        vendor,
        device,
        index,
    })
    .unwrap_or_else(|e| fail(e));
    let gpu = &gpus[0];
    let bar = gpu
        .bars
        .iter()
        .copied()
        .max_by_key(|bar| bar.len)
        .unwrap_or_else(|| fail("no memory BAR".into()));
    let ram = end_of_ram().unwrap_or_else(|| fail("no MemTotal in /proc/meminfo".into()));
    println!(
        "{NAME}: bar {}: {:#x} {:#x} (RAM ends below {ram:#x})",
        bar.index, bar.phys, bar.len
    );
    ok(
        "its largest BAR is past the machine's RAM, where the boot mapped nothing",
        bar.phys >= ram && bar.len >= 2 * IMAGE,
    );
    println!(
        "{NAME}: it is {} 512 GiB: below {} top-level entry of the kernel's tables",
        if bar.phys >= 512 * GIB {
            "above"
        } else {
            "below"
        },
        if bar.phys >= 512 * GIB {
            "its own"
        } else {
            "RAM's"
        }
    );
    // A pointer's image at the end of the BAR, drawn as a driver draws
    // one: through its own mapping.
    let deep = bar.phys + bar.len - IMAGE;
    let mine = gpu.mapper().query_or_map(deep, IMAGE);
    ok(
        "the driver maps the end of that BAR for itself",
        mine.is_some(),
    );
    let pixels = mine.unwrap_or(0) as *mut u32;
    // SAFETY: `IMAGE` bytes of the BAR, mapped just above.
    let drawn = unsafe {
        for i in 0..IMAGE / 4 {
            pixels.add(i).write_volatile(0xff00_0000 | i as u32);
        }
        pixels.add(IMAGE / 4 - 1).read_volatile() == 0xff00_0000 | (IMAGE / 4 - 1) as u32
    };
    ok("...and draws a pointer's image there", drawn);
    let slice = gem_mmap::alloc_handle_slice();
    ok(
        "the kernel gives the driver a slice of GEM handles",
        slice != HandleSlice::exhausted(),
    );
    let (h, deep, image) = (slice.base(), deep as u64, IMAGE as u64);
    if asked {
        ok(
            "the kernel has a window onto that range, when asked",
            gem_mmap::window(deep, image),
        );
    }
    gem_mmap::register_device_memory(h, deep, image, std::process::id() as u64);
    ok(
        if asked {
            "a GEM object there is device memory in the kernel's table"
        } else {
            "a GEM object there, with no window asked for, is device memory in the kernel's table"
        },
        gem_mmap::is_device_memory(h),
    );
    match pointer_image_from(h) {
        Some(read) => ok(
            "the kernel reads it where it is, for the pointer's image",
            read,
        ),
        None => fail("no node of /dev/dri has a CRTC: the kernel's read cannot be tried".into()),
    }
    // That process holds the object as a client that imported it does.
    ok(
        "the process that was running before the kernel had the window holds it too",
        gem_mmap::add_ref(h, earlier.pid as u64).is_some(),
    );
    match earlier.read(h) {
        Ok(read) => ok("...and the kernel reads it there as well", read),
        Err(e) => fail(format!(
            "the kernel's read in a process that was running before: {e}"
        )),
    }
    println!("{NAME}: window-check: ok");
}
