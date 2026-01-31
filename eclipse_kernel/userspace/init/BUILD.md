# Eclipse Init Build Instructions

## Building the Init Process

The init process must be built **before** building the kernel, as the kernel embeds the init binary using `include_bytes!`.

### Build Order:

```bash
# 1. Build init first
cd eclipse_kernel/userspace/init
cargo +nightly build --release

# 2. Then build kernel (which embeds init)
cd ../..
cargo +nightly build --release --target x86_64-unknown-none
```

### Build Requirements:
- Rust nightly toolchain
- `x86_64-unknown-none` target installed
- `rust-src` component for building core library

### Installing Requirements:
```bash
rustup toolchain install nightly
rustup target add x86_64-unknown-none --toolchain nightly
rustup component add rust-src --toolchain nightly
```

## Files:
- `src/main.rs` - Init process code
- `Cargo.toml` - Package configuration  
- `.cargo/config.toml` - Build configuration (target, build-std)
- `linker.ld` - Linker script for userspace programs

## Binary Output:
- Location: `target/x86_64-unknown-none/release/eclipse-init`
- Format: Static ELF 64-bit executable
- Size: ~11 KB
- Load Address: 0x400000 (4 MB)

## Integration with Kernel:
The kernel embeds this binary at compile time:
```rust
static INIT_BINARY: &[u8] = include_bytes!("../userspace/init/target/x86_64-unknown-none/release/eclipse-init");
```

This means init must be built before the kernel build can succeed.
