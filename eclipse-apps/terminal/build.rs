// Script de compilación para proporcionar la ruta absoluta del linker script.
// Se ejecuta en el host durante la compilación cruzada; usa CARGO_MANIFEST_DIR
// para construir una ruta portátil sin depender del directorio de trabajo del enlazador.
fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    // El linker script está en la raíz del workspace (un nivel arriba del paquete)
    let linker_script = format!("{}/../linker.ld", manifest_dir);
    println!("cargo:rustc-link-arg=-T{}", linker_script);
    println!("cargo:rerun-if-changed=../linker.ld");
}
