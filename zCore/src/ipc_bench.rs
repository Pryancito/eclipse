//! Kernel-side channel call/reply benchmark: the IPC baseline for the
//! microkernel work, measured on the real executor.
//!
//! Activated with `eclipse.ipcbench=N` on the kernel cmdline (N = round trips
//! per case). A client task makes `N` `Channel::call`s of a 32-byte message
//! and a server task answers each one, so every round trip pays what a
//! userspace server will pay apart from the trap: two wake-ups through the
//! executor and two task switches. Two cases:
//!
//! * `same-cpu`: both tasks pinned to CPU 0. This is the path the planned
//!   direct hand-off (client gives its CPU straight to the server) replaces.
//! * `cross-cpu`: client on CPU 0, server on CPU 1 (skipped with one CPU),
//!   which adds the IPI and remote run-queue cost.
//! * `unpinned`: neither task pinned, where the scheduler places them; this
//!   is how ordinary threads run, and pinned tasks take the scheduler's
//!   slower affinity path.
//!
//! The cases start 20 s after boot (so init's start-up does not share the
//! CPUs with them) and run three rounds. Results go to the serial console and dmesg as
//! `ipcbench: <case> <N> calls, <ns> ns/call`, and `ipcbench: done` at the end.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use core::time::Duration;

use kernel_hal::timer::timer_now;
use zircon_object::ipc::{Channel, MessagePacket};
use zircon_object::object::{KernelObject, Signal};

/// How long after the call to [`start`] the first case runs.
const SETTLE: Duration = Duration::from_secs(20);

/// How many times each case is run; the spread says how far to trust one.
const ROUNDS: u32 = 3;

/// The round-trip count `eclipse.ipcbench=N` asks for. `None` = disabled.
pub use zcore_boot_opts::parse_ipcbench;

fn bench_log(msg: core::fmt::Arguments) {
    kernel_hal::console::serial_write_fmt_spin(format_args!("\n{}\n", msg));
    klog_info!("{}", msg);
}

fn message() -> MessagePacket {
    MessagePacket {
        data: alloc::vec![0u8; 32],
        handles: alloc::vec::Vec::new(),
    }
}

/// Answer every call on `end` until the peer goes away.
async fn server(end: Arc<Channel>) {
    let object: Arc<dyn KernelObject> = end.clone();
    loop {
        match end.read() {
            Ok(request) => {
                // The request carries the txid in its first 4 bytes; echoing
                // it back is the reply.
                if end.write(request).is_err() {
                    return;
                }
            }
            Err(zircon_object::ZxError::SHOULD_WAIT) => {
                let signal = object
                    .wait_signal(Signal::READABLE | Signal::PEER_CLOSED)
                    .await;
                if signal.contains(Signal::PEER_CLOSED) && !signal.contains(Signal::READABLE) {
                    return;
                }
            }
            Err(_) => return,
        }
    }
}

/// Run `n` calls on `end` and answer the mean time per round trip.
async fn client(end: &Arc<Channel>, n: u64) -> Option<u64> {
    // Warm-up: first wake-ups pay for cold caches and run-queue setup.
    for _ in 0..(n / 10).max(1) {
        end.call(message()).await.ok()?;
    }
    let start = timer_now();
    for _ in 0..n {
        end.call(message()).await.ok()?;
    }
    let elapsed: Duration = timer_now() - start;
    Some(elapsed.as_nanos() as u64 / n)
}

/// Spawn `fut` pinned to `cpu`, or free to run anywhere with `None`.
fn spawn_on(fut: impl core::future::Future<Output = ()> + Send + 'static, cpu: Option<usize>) {
    match cpu {
        Some(cpu) => {
            kernel_hal::thread::spawn_with_affinity(fut, Arc::new(AtomicU64::new(1 << cpu)))
        }
        None => kernel_hal::thread::spawn(fut),
    }
}

/// Run one case: a server and a client task placed as asked, the driver
/// waiting (asleep, off both CPUs' run queues) until the client is done.
async fn run_case(
    name: &'static str,
    n: u64,
    client_cpu: Option<usize>,
    server_cpu: Option<usize>,
) {
    let (client_end, server_end) = Channel::create();
    spawn_on(server(server_end), server_cpu);
    let done = Arc::new(AtomicBool::new(false));
    let flag = done.clone();
    spawn_on(
        async move {
            match client(&client_end, n).await {
                Some(ns) => bench_log(format_args!(
                    "ipcbench: {} {} calls, {} ns/call",
                    name, n, ns
                )),
                None => bench_log(format_args!(
                    "ipcbench: {} FAILED (call returned an error)",
                    name
                )),
            }
            // Dropping the client end closes the channel and ends the server.
            drop(client_end);
            flag.store(true, Ordering::Release);
        },
        client_cpu,
    );
    while !done.load(Ordering::Acquire) {
        kernel_hal::thread::sleep_until(timer_now() + Duration::from_millis(100)).await;
    }
}

/// Spawn the benchmark. Call once after SMP is up and the executor is running.
pub fn start(n: usize) {
    let n = n as u64;
    let online = kernel_hal::online_cpu_count().max(1);
    bench_log(format_args!(
        "Eclipse: IPC bench ON (eclipse.ipcbench={}) on {} CPU(s)",
        n, online
    ));
    spawn_on(
        async move {
            // Let the boot (init, its services) settle first: the numbers are
            // meant to be the IPC path's, not the boot's contention.
            kernel_hal::thread::sleep_until(timer_now() + SETTLE).await;
            for round in 1..=ROUNDS {
                bench_log(format_args!("ipcbench: round {}/{}", round, ROUNDS));
                run_case("same-cpu", n, Some(0), Some(0)).await;
                // CPU 1 itself must be up: more than one CPU online does not
                // say which (a partial SMP bring-up can leave CPU 1 out), and
                // a server pinned to a CPU that never runs never answers.
                if kernel_hal::cpu_online_mask() & 0b10 != 0 {
                    run_case("cross-cpu", n, Some(0), Some(1)).await;
                } else {
                    bench_log(format_args!("ipcbench: cross-cpu skipped (CPU 1 offline)"));
                }
                run_case("unpinned", n, None, None).await;
            }
            bench_log(format_args!("ipcbench: done"));
        },
        None,
    );
}
