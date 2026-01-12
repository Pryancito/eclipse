use std::process::Command;

fn main() {
    // Compilar el archivo assembly para cambio de contexto usando nasm
    println!("cargo:rerun-if-changed=src/context_switch.asm");

    let out_dir = std::env::var("OUT_DIR").unwrap();
    let obj_file = format!("{}/context_switch.o", out_dir);
    let lib_file = format!("{}/libcontext_switch.a", out_dir);

    // Compilar con nasm
    let status = Command::new("nasm")
        .args(&["-f", "elf64", "-o", &obj_file, "src/context_switch.asm"])
        .status()
        .expect("Failed to run nasm");

    if !status.success() {
        panic!("nasm failed to assemble context_switch.asm");
    }

    // Crear una biblioteca estática a partir del archivo objeto
    let ar_status = Command::new("ar")
        .args(&["rcs", &lib_file, &obj_file])
        .status()
        .expect("Failed to run ar");

    if !ar_status.success() {
        panic!("ar failed to create static library");
    }

    // Decir a cargo que incluya la biblioteca
    println!("cargo:rustc-link-search=native={}", out_dir);
    println!("cargo:rustc-link-lib=static=context_switch");

    // --- Compilar Trampoline para SMP ---
    println!("cargo:rerun-if-changed=src/platform/trampoline.asm");
    let tram_obj = format!("{}/trampoline.o", out_dir);
    let tram_lib = format!("{}/libtrampoline.a", out_dir);

    let tram_status = Command::new("nasm")
        .args(&["-f", "elf64", "-o", &tram_obj, "src/platform/trampoline.asm"])
        .status()
        .expect("Failed to run nasm for trampoline");

    if !tram_status.success() {
        panic!("nasm failed to assemble trampoline.asm");
    }

    let tram_ar = Command::new("ar")
        .args(&["rcs", &tram_lib, &tram_obj])
        .status()
        .expect("Failed to run ar for trampoline");
    
    if !tram_ar.success() {
        panic!("ar failed to create static library for trampoline");
    }

    println!("cargo:rustc-link-lib=static=trampoline");
}
