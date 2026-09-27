//! Runs `src/srcs_mk.rs`'s own tests.
//!
//! That file is what `build.rs` uses to turn NVIDIA's `srcs.mk` into the list of
//! C files it compiles, and it is deliberately not a module of the library:
//! nothing in the kernel needs it. Which also means no `cargo test` would
//! compile it, and the checks it makes -- an override that stopped overriding, a
//! source assigned with a spelling the parser drops -- would never run.
//! Including it here is what runs them.
//!
//! `build.rs` itself cannot be included instead: it uses `cc`, which is a build
//! dependency and is not on a test's dependency list.

#[path = "../src/srcs_mk.rs"]
mod srcs_mk;
