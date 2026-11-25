// The lock sources pick loom's atomics under `cfg(loom)` (`../src/prims.rs`).
// Setting it here, for this crate alone, rather than through RUSTFLAGS keeps
// it off loom's own dependencies and lets a plain `cargo test` run the models.
fn main() {
    println!("cargo:rustc-cfg=loom");
}
