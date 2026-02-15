fn main() {
    use std::env;
    use std::path::PathBuf;

    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let linker_script = PathBuf::from(manifest_dir).join("linker.ld");

    println!("cargo:rustc-link-arg=-T{}", linker_script.display());
    println!("cargo:rerun-if-changed=linker.ld");
}
