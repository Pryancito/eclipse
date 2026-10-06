//! Uart device driver.

mod buffered;
mod uart_16550;

pub use buffered::BufferedUart;
pub use uart_16550::{reentrant_console_writes, Uart16550Mmio};

#[cfg(target_arch = "x86_64")]
pub use uart_16550::Uart16550Pmio;

// The PL011 is aarch64 hardware, but nothing in it is aarch64 code: it is
// volatile reads and writes at `base + offset`, which ordinary memory answers
// just as well. Gating the module on `target_arch` alone meant no `cargo test`
// invocation anywhere compiled it -- the host suite is the only thing that runs
// tests -- so a test in here would have been a test nothing runs. `cfg(test)`
// lets it into the host build, where its tests actually execute.
#[cfg(any(target_arch = "aarch64", test))]
mod uart_pl011;

#[cfg(any(target_arch = "aarch64", test))]
pub use uart_pl011::Pl011Uart;

#[cfg(feature = "allwinner")]
mod uart_allwinner;

#[cfg(feature = "allwinner")]
pub use uart_allwinner::UartAllwinner;

// Same as the PL011 above: gated on a feature no `cargo test` line turns on, so
// nothing compiled it -- not even to check it against `deny(warnings)`.
#[cfg(any(feature = "fu740", test))]
mod uart_u740;

#[cfg(any(feature = "fu740", test))]
pub use uart_u740::UartU740Mmio;
