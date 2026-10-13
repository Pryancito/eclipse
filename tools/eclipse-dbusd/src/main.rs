//! The `eclipse-dbusd` binary: nothing but the entry point.
//!
//! Everything the daemon does lives in the library (`src/lib.rs` and the
//! modules beside it) so that the second binary of this package,
//! `eclipse-portald`, can reuse the wire format (`message`) and the blocking
//! client (`client`) instead of carrying a second copy of a marshaller.

fn main() {
    eclipse_dbusd::cli_main();
}
