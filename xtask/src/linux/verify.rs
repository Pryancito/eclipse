//! Verificación de la rootfs ya construida, antes de empaquetarla en imagen.
//!
//! El motivo es concreto, no hipotético: los tres fallos que dejaron una imagen
//! de arm64 inservible eran **silenciosos**.
//!
//! - Las herramientas de `tools/` se enlazaron con el `ld` del host, así que
//!   `eclipse-init` no se construyó y la imagen salió con busybox de PID 1.
//!   `install_eclipse_init` lo dice con un `warning:` y devuelve `false`, pero
//!   la construcción sigue y termina «bien»: en medio minuto de salida, nadie
//!   ve ese aviso.
//! - `/etc/apk/arch` no se escribía, así que apk resolvía contra el índice del
//!   host.
//! - Las claves de firma no iban por arco, así que apk decía
//!   `UNTRUSTED signature` y **no instalaba nada, sin fallar**.
//!
//! Los tres se ven mirando la rootfs terminada, sin red y sin arrancar nada:
//! un ELF lleva su máquina en la cabecera, y los dos ficheros de apk o están o
//! no están. Esto es esa mirada, y **falla** en vez de avisar.
//!
//! Escotilla: `ECLIPSE_ALLOW_INCOMPLETE_ROOTFS=1` degrada los fallos a avisos,
//! para quien quiera a propósito una imagen a medias (p. ej. depurar el propio
//! arranque de la rootfs con busybox de init).

use crate::arch::Arch;
use crate::{LinuxRootfs, PROJECT_DIR};
use std::{
    ffi::{OsStr, OsString},
    fs,
    io::Read,
    path::Path,
};

/// La escotilla, por si alguien quiere a propósito una imagen incompleta.
const ALLOW_INCOMPLETE: &str = "ECLIPSE_ALLOW_INCOMPLETE_ROOTFS";

/// `e_machine` de la cabecera ELF que tiene que llevar todo binario propio de
/// la rootfs. Los valores son los de `elf.h`.
const EM_X86_64: u16 = 62;
const EM_AARCH64: u16 = 183;
const EM_RISCV: u16 = 243;

/// Lo que se devuelve cuando el fichero SI es un ELF pero no uno de los
/// nuestros: clase distinta de ELF64 o big endian. No es `None` --- «no es un
/// ELF» se salta en silencio, y esto no se salta: ninguna maquina esperada
/// vale `u16::MAX`, asi que sale en el informe.
///
/// El caso concreto que lo hizo falta: RV32 y RV64 comparten `EM_RISCV`, asi
/// que mirando solo la maquina un binario de 32 bits pasaba por bueno en una
/// rootfs de riscv64.
const MACHINE_RARA: u16 = u16::MAX;

/// Lo que cada binario de la rootfs tiene que declarar para este arco.
pub(super) const fn expected_machine(arch: Arch) -> u16 {
    match arch {
        Arch::X86_64 => EM_X86_64,
        Arch::Aarch64 => EM_AARCH64,
        Arch::Riscv64 => EM_RISCV,
    }
}

/// El nombre legible de un `e_machine`, para que el informe diga qué salió en
/// vez de un número.
fn machine_name(machine: u16) -> &'static str {
    match machine {
        EM_X86_64 => "x86_64",
        EM_AARCH64 => "aarch64",
        EM_RISCV => "riscv",
        3 => "i386",
        40 => "arm",
        MACHINE_RARA => "una clase o endianness que no construimos",
        _ => "desconocida",
    }
}

/// Lee el `e_machine` de un fichero, o `None` si no es un ELF.
///
/// Lee **solo los 20 primeros bytes**, no el fichero entero: esto lo llama
/// `check_elf_machines` por cada fichero regular de la rootfs, y una rootfs de
/// escritorio lleva firmware y assets de cientos de MB que no hay por que
/// cargar en memoria para mirar una cabecera.
///
/// Un script, una imagen o un fichero de texto no son un ELF y se saltan sin
/// ruido --- no es un error que `/etc/profile` no tenga cabecera. Lo que si es
/// un ELF pero no de los nuestros (ELF32, big endian) sale como
/// [`MACHINE_RARA`] y se reporta.
pub(super) fn elf_machine(path: &Path) -> Option<u16> {
    let mut header = [0u8; 20];
    fs::File::open(path).ok()?.read_exact(&mut header).ok()?;
    if &header[..4] != b"\x7fELF" {
        return None;
    }
    // EI_CLASS: 2 = ELF64, y los tres arcos que soportamos son de 64 bits.
    // EI_DATA: 1 = little endian, que es lo unico que construimos.
    if header[4] != 2 || header[5] != 1 {
        return Some(MACHINE_RARA);
    }
    Some(u16::from_le_bytes([header[18], header[19]]))
}

/// Los `.pub` que hay en un directorio, por nombre.
fn claves_pub(dir: &Path) -> Vec<OsString> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("pub"))
        .map(|e| e.file_name())
        .collect()
}

/// Los nombres de clave que ESTE arco necesita: los `.pub` de
/// `tools/apk/keys/<arco>/` y `prebuilt/alpine-apk-keys/<arco>/`, que son
/// exactamente los que `install_apk_keys` cuenta como «del arco».
///
/// Sale vacio si no hay fuente de la que sacarlos, y entonces no se exige
/// nada: el verificador no puede inventarse el nombre de una clave de Alpine.
fn claves_del_arco(arch: &str) -> Vec<OsString> {
    let mut out = Vec::new();
    for base in [
        PROJECT_DIR.join("tools").join("apk").join("keys"),
        PROJECT_DIR.join("prebuilt").join("alpine-apk-keys"),
    ] {
        out.extend(claves_pub(&base.join(arch)));
    }
    out
}

/// Un problema encontrado en la rootfs, con la frase que se le enseña a quien
/// construyó.
struct Problem(String);

impl LinuxRootfs {
    /// Mira la rootfs terminada y aborta si le falta algo que la haría
    /// arrancar a medias en silencio.
    ///
    /// Llamada desde `image()` justo después de `make(false)`, para que un
    /// `make image` la ejecute sin que nadie se acuerde de pedirla, y expuesta
    /// como `cargo verify-rootfs` para poder correrla sola.
    pub fn verify(&self) {
        let root = self.path();
        println!(
            "=== Verificando rootfs: {arch}, variante {variant} ({path}) ===",
            arch = self.0.name(),
            variant = self.variant().name(),
            path = root.display(),
        );

        if !root.is_dir() {
            panic!(
                "no hay rootfs que verificar en {}: construyela antes (`cargo rootfs --arch {}`)",
                root.display(),
                self.0.name()
            );
        }

        let mut problems = Vec::new();
        problems.extend(self.check_init(&root));
        problems.extend(self.check_apk(&root));
        problems.extend(self.check_elf_machines(&root));

        if problems.is_empty() {
            println!(
                "rootfs verificada: PID 1, apk y los ELF cuadran con {}.",
                self.0.name()
            );
            return;
        }

        let allowed = std::env::var_os(ALLOW_INCOMPLETE).is_some_and(|v| v == "1");
        let verb = if allowed { "warning" } else { "error" };
        for Problem(p) in &problems {
            eprintln!("{verb}: rootfs: {p}");
        }
        if allowed {
            eprintln!(
                "warning: {} problema(s) en la rootfs, tolerados por {ALLOW_INCOMPLETE}=1",
                problems.len()
            );
            return;
        }
        panic!(
            "la rootfs de {} tiene {} problema(s) que harian arrancar la imagen a medias; \
             arreglalos, o pon {ALLOW_INCOMPLETE}=1 si los quieres a proposito",
            self.0.name(),
            problems.len(),
        );
    }

    /// PID 1: que `/sbin/init` lleve a `eclipse-init` y que el binario esté.
    ///
    /// busybox init sigue siendo la red de seguridad **en el arranque**, y eso
    /// no cambia: lo que no vale es *distribuir* una imagen que cayó en la red
    /// de seguridad sin que nadie se enterase.
    fn check_init(&self, root: &Path) -> Vec<Problem> {
        let mut out = Vec::new();
        let sbin = root.join("sbin");
        let eclipse_init = sbin.join("eclipse-init");
        if !eclipse_init.is_file() {
            out.push(Problem(
                "falta /sbin/eclipse-init: su compilacion cruzada no llego, asi que la imagen \
                 lleva busybox de PID 1 (es el fallo que dejo la ISO de arm64 sin init)"
                    .into(),
            ));
            // Sin el binario, el enlace no puede apuntar a él: un problema basta.
            return out;
        }

        let init = sbin.join("init");
        match fs::read_link(&init) {
            Ok(target) if target.as_os_str() == "eclipse-init" => {}
            Ok(target) => out.push(Problem(format!(
                "/sbin/init apunta a {} y no a eclipse-init, que si esta en la rootfs",
                target.display()
            ))),
            Err(e) => out.push(Problem(format!(
                "/sbin/init no es un enlace a eclipse-init ({e}), aunque el binario esta"
            ))),
        }

        let svc = root.join("etc").join("eclipse").join("services");
        if !svc.is_dir() {
            out.push(Problem(
                "falta /etc/eclipse/services: eclipse-init arrancaria sin un solo servicio".into(),
            ));
        }
        out
    }

    /// apk: el arco y las claves, los dos fallos que dejaban a apk sin instalar
    /// nada y sin decirlo.
    fn check_apk(&self, root: &Path) -> Vec<Problem> {
        let mut out = Vec::new();
        let etc_apk = root.join("etc").join("apk");

        let arch_file = etc_apk.join("arch");
        match fs::read_to_string(&arch_file) {
            Ok(s) if s.trim() == self.0.name() => {}
            Ok(s) => out.push(Problem(format!(
                "/etc/apk/arch dice {:?} y la rootfs es de {}: apk resolveria contra el indice \
                 equivocado",
                s.trim(),
                self.0.name()
            ))),
            Err(_) => out.push(Problem(
                "falta /etc/apk/arch: apk resuelve contra el indice del host, no del objetivo"
                    .into(),
            )),
        }

        let keys = etc_apk.join("keys");
        let instaladas = claves_pub(&keys);
        if instaladas.is_empty() {
            out.push(Problem(
                "/etc/apk/keys no tiene una sola clave .pub: apk dice UNTRUSTED signature y no \
                 instala nada, en silencio"
                    .into(),
            ));
            return out;
        }

        // Que el directorio tenga ALGO no basta, y es justo el caso que
        // `install_apk_keys` avisa y nadie lee: con las claves de otro arco
        // ahi dentro el contador sale distinto de cero, nadie pasa
        // `--allow-untrusted`, y apk se encuentra el APKINDEX del objetivo
        // firmado por una clave que no tiene. Asi que se exige por NOMBRE una
        // de las del arco.
        let necesarias = claves_del_arco(self.0.name());
        if !necesarias.is_empty() && !necesarias.iter().any(|k| instaladas.contains(k)) {
            out.push(Problem(format!(
                "/etc/apk/keys tiene {} clave(s) pero ninguna de {}: el APKINDEX de {} saldra \
                 «UNTRUSTED signature» y no se instalara ni un paquete. Las de cada arco van en \
                 tools/apk/keys/{}/",
                instaladas.len(),
                self.0.name(),
                self.0.name(),
                self.0.name(),
            )));
        }
        out
    }

    /// Que ningún ELF de la rootfs sea del arco equivocado.
    ///
    /// Esta es la comprobación que caza el `ld` del host directamente: un
    /// binario construido para el anfitrión lleva su `e_machine` en la
    /// cabecera y la rootfs es del objetivo, así que no hay ambigüedad.
    ///
    /// Se salta `libc-test/` y `other-test/`, que son suites traídas de fuera y
    /// no las construimos nosotros.
    fn check_elf_machines(&self, root: &Path) -> Vec<Problem> {
        let want = expected_machine(self.0);
        let mut out = Vec::new();
        let mut checked = 0usize;
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let entries = match fs::read_dir(&dir) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                let name = entry.file_name();
                // Suites ajenas y rutas que no son ficheros nuestros.
                if matches!(
                    name.to_str(),
                    Some("libc-test") | Some("other-test") | Some("proc") | Some("sys")
                ) {
                    continue;
                }
                // `lib/firmware/` no es userspace del objetivo: son blobs para
                // el procesador de un periferico. El `gsp.bin` de la GSP es un
                // ELF de RISC-V, y lo instala `image()` DESPUES de verificar,
                // asi que la siguiente construccion --- que preserva la rootfs
                // --- lo encontraria y enrojeceria una imagen de x86_64
                // perfectamente sana. Dicho de otra forma: sin esto, el
                // verificador pasa la primera vez y falla la segunda.
                if name == "firmware" && dir.file_name() == Some(OsStr::new("lib")) {
                    continue;
                }
                // `file_type()` de `read_dir` NO sigue los enlaces, asi que un
                // enlace no es ni `is_dir` ni `is_file` y cae al brazo de
                // abajo: ni se sigue ni se lee. Eso es lo que se quiere --- el
                // destino se resuelve dentro del sistema arrancado, no desde
                // aqui ---, y por eso no hay un brazo para `is_symlink`: seria
                // inalcanzable. `los_enlaces_no_se_siguen` lo fija.
                match entry.file_type() {
                    Ok(t) if t.is_dir() => stack.push(path),
                    Ok(t) if t.is_file() => {
                        if let Some(machine) = elf_machine(&path) {
                            checked += 1;
                            if machine != want {
                                out.push(Problem(format!(
                                    "{} es un ELF de {} y la rootfs es de {}: se enlazo con el \
                                     toolchain equivocado",
                                    path.strip_prefix(root).unwrap_or(&path).display(),
                                    machine_name(machine),
                                    self.0.name(),
                                )));
                            }
                        }
                    }
                    _ => {}
                }
                // Un informe con cien lineas no se lee. Con las primeras basta
                // para saber que el toolchain estaba mal.
                if out.len() >= 10 {
                    out.push(Problem(
                        "... y mas ELF del arco equivocado (informe cortado en 10)".into(),
                    ));
                    return out;
                }
            }
        }
        println!("ELF mirados: {checked}, esperando {}", machine_name(want));
        if checked == 0 {
            out.push(Problem(
                "no hay un solo ELF en la rootfs: no se ha construido nada de userspace".into(),
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Una cabecera ELF64 little endian mínima con la máquina pedida, que es
    /// todo lo que `elf_machine` mira.
    fn elf64(machine: u16) -> Vec<u8> {
        let mut v = vec![0u8; 64];
        v[..4].copy_from_slice(b"\x7fELF");
        v[4] = 2; // ELFCLASS64
        v[5] = 1; // ELFDATA2LSB
        v[18..20].copy_from_slice(&machine.to_le_bytes());
        v
    }

    fn scratch(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("eclipse-verify-tests");
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn lee_la_maquina_de_un_elf64() {
        let p = scratch("x86.elf", &elf64(62));
        assert_eq!(elf_machine(&p), Some(62));
        let p = scratch("arm.elf", &elf64(183));
        assert_eq!(elf_machine(&p), Some(183));
    }

    /// El caso que importa: el binario del host dentro de una rootfs cruzada.
    #[test]
    fn un_elf_del_host_no_cuadra_con_el_objetivo() {
        let p = scratch("host.elf", &elf64(62));
        assert_eq!(elf_machine(&p), Some(62));
        assert_ne!(elf_machine(&p), Some(expected_machine(Arch::Aarch64)));
    }

    #[test]
    fn lo_que_no_es_elf_se_salta() {
        assert_eq!(
            elf_machine(&scratch("script.sh", b"#!/bin/sh\necho hola\n")),
            None
        );
        // Un fichero mas corto que la cabecera no se lee fuera de rango.
        assert_eq!(elf_machine(&scratch("corto", b"\x7fELF")), None);
        assert_eq!(elf_machine(&scratch("vacio", b"")), None);
    }

    /// Un ELF big endian no se cuela como «no es un ELF»: ninguna maquina
    /// esperada es `u16::MAX`, asi que se reporta.
    #[test]
    fn un_elf_big_endian_no_pasa_por_bueno() {
        let mut v = elf64(EM_AARCH64);
        v[5] = 2; // ELFDATA2MSB
        let p = scratch("be.elf", &v);
        assert_eq!(elf_machine(&p), Some(u16::MAX));
        for arch in [Arch::X86_64, Arch::Aarch64, Arch::Riscv64] {
            assert_ne!(expected_machine(arch), u16::MAX);
        }
    }

    /// Los numeros van **literales**, no por la constante: comparar
    /// `expected_machine(Aarch64)` con `EM_AARCH64` no comprueba nada, y un
    /// `e_machine` equivocado haria pasar por ajeno a TODO binario del arco
    /// correcto --- es decir, rompe la imagen entera en vez de dejar colar
    /// una. Los valores son los de `elf.h` y no cambian nunca.
    /// `cached_for_target` es la puerta que ahora llevan las 21 cachés de
    /// artefactos de `mod.rs`. El caso que la hizo falta: el x86_64 construye
    /// `tools/eclipse-resolv/libeclipse_dns.so`, el aarch64 lo encuentra más
    /// nuevo que su `.c` y se lo lleva a la rootfs. Lo cazó
    /// `cargo verify-rootfs` la primera vez que corrió en CI.
    #[test]
    fn una_cache_de_otro_arco_no_se_reutiliza() {
        let arm = LinuxRootfs::new(Arch::Aarch64);
        let x86 = LinuxRootfs::new(Arch::X86_64);
        let del_host = scratch("cache-x86.so", &elf64(62));
        assert!(
            !arm.cached_for_target(&del_host),
            "el de x86_64 no vale en arm"
        );
        assert!(x86.cached_for_target(&del_host), "en x86_64 si vale");

        let del_arm = scratch("cache-arm.so", &elf64(183));
        assert!(arm.cached_for_target(&del_arm));
        assert!(!x86.cached_for_target(&del_arm));
    }

    /// Lo que no es un ELF no dice nada del arco, asi que sigue valiendo: un
    /// script o un asset en la cache no tiene que forzar una recompilacion.
    #[test]
    fn lo_que_no_es_elf_sigue_valiendo_en_la_cache() {
        let arm = LinuxRootfs::new(Arch::Aarch64);
        assert!(arm.cached_for_target(&scratch("cache.sh", b"#!/bin/sh\nexit 0\n")));
        // Y un fichero que no existe tampoco: de eso ya se encarga el
        // `is_file()` de cada puerta.
        assert!(arm.cached_for_target(std::path::Path::new("/no/existe/nada")));
    }

    /// Una rootfs de mentira, completa y sana, para el arco pedido.
    ///
    /// Cada test la rompe por un sitio y comprueba que la rotura sale, que es
    /// lo contrario de comprobar que una rootfs buena pasa: lo que fallo en
    /// arm64 fue que una rootfs ROTA pasaba.
    fn rootfs_sana(nombre: &str, arch: Arch, machine: u16) -> std::path::PathBuf {
        let root = std::env::temp_dir()
            .join("eclipse-verify-rootfs")
            .join(nombre);
        let _ = fs::remove_dir_all(&root);
        let sbin = root.join("sbin");
        fs::create_dir_all(&sbin).unwrap();
        fs::write(sbin.join("eclipse-init"), elf64(machine)).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("eclipse-init", sbin.join("init")).unwrap();
        fs::create_dir_all(root.join("etc").join("eclipse").join("services")).unwrap();
        let apk = root.join("etc").join("apk");
        fs::create_dir_all(apk.join("keys")).unwrap();
        fs::write(apk.join("arch"), format!("{}\n", arch.name())).unwrap();
        // Una clave DE ESTE ARCO, con su nombre de verdad: con cualquier otro
        // nombre la rootfs no estaria sana, que es justo lo que ahora se mira.
        fs::write(apk.join("keys").join(clave_del_arco(arch)), b"clave").unwrap();
        root
    }

    /// El nombre de una clave real del arco, sacado del arbol como lo hace el
    /// verificador.
    fn clave_del_arco(arch: Arch) -> OsString {
        claves_del_arco(arch.name())
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("tools/apk/keys/{}/ sin claves .pub", arch.name()))
    }

    #[test]
    fn una_rootfs_sana_no_tiene_problemas() {
        let r = LinuxRootfs::new(Arch::X86_64);
        let root = rootfs_sana("sana", Arch::X86_64, 62);
        assert!(r.check_init(&root).is_empty());
        assert!(r.check_apk(&root).is_empty());
        assert!(r.check_elf_machines(&root).is_empty());
    }

    /// EL fallo de arm64: `eclipse-init` no se construyo y la imagen salio con
    /// busybox de PID 1, con un `warning:` por toda señal.
    #[test]
    fn sin_eclipse_init_se_acusa() {
        let r = LinuxRootfs::new(Arch::X86_64);
        let root = rootfs_sana("sin-init", Arch::X86_64, 62);
        fs::remove_file(root.join("sbin").join("eclipse-init")).unwrap();
        let p = r.check_init(&root);
        assert_eq!(p.len(), 1, "un solo problema, no dos por el enlace colgado");
        assert!(p[0].0.contains("eclipse-init"), "{}", p[0].0);
    }

    #[test]
    fn un_init_que_apunta_a_busybox_se_acusa() {
        let r = LinuxRootfs::new(Arch::X86_64);
        let root = rootfs_sana("init-busybox", Arch::X86_64, 62);
        let init = root.join("sbin").join("init");
        fs::remove_file(&init).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("../bin/busybox", &init).unwrap();
        let p = r.check_init(&root);
        assert_eq!(p.len(), 1);
        assert!(p[0].0.contains("busybox"), "{}", p[0].0);
    }

    /// El segundo fallo silencioso: apk resolviendo contra el indice del host.
    #[test]
    fn un_apk_arch_equivocado_o_ausente_se_acusa() {
        let r = LinuxRootfs::with_variant(Arch::Aarch64, crate::variant::Variant::Desktop);
        let root = rootfs_sana("apk-arch", Arch::Aarch64, 183);
        let arch_file = root.join("etc").join("apk").join("arch");
        fs::write(&arch_file, b"x86_64\n").unwrap();
        let p = r.check_apk(&root);
        assert_eq!(p.len(), 1);
        assert!(
            p[0].0.contains("x86_64") && p[0].0.contains("aarch64"),
            "{}",
            p[0].0
        );

        fs::remove_file(&arch_file).unwrap();
        let p = r.check_apk(&root);
        assert_eq!(p.len(), 1);
        assert!(p[0].0.contains("falta /etc/apk/arch"), "{}", p[0].0);
    }

    /// El tercero: sin claves del arco, apk dice UNTRUSTED y no instala nada.
    #[test]
    fn sin_claves_de_apk_se_acusa() {
        let r = LinuxRootfs::new(Arch::X86_64);
        let root = rootfs_sana("sin-claves", Arch::X86_64, 62);
        fs::remove_file(
            root.join("etc")
                .join("apk")
                .join("keys")
                .join(clave_del_arco(Arch::X86_64)),
        )
        .unwrap();
        let p = r.check_apk(&root);
        assert_eq!(p.len(), 1);
        assert!(p[0].0.contains("UNTRUSTED"), "{}", p[0].0);
    }

    /// El caso que el contador de entradas no veia: el directorio tiene
    /// claves, pero son de OTRO arco. `install_apk_keys` ya lo avisaba por
    /// `eprintln!`; esto lo convierte en un fallo.
    #[test]
    fn con_claves_de_otro_arco_se_acusa() {
        let r = LinuxRootfs::new(Arch::Aarch64);
        let root = rootfs_sana("claves-de-otro-arco", Arch::Aarch64, 183);
        let keys = root.join("etc").join("apk").join("keys");
        fs::remove_file(keys.join(clave_del_arco(Arch::Aarch64))).unwrap();
        fs::write(keys.join(clave_del_arco(Arch::X86_64)), b"clave").unwrap();
        let p = r.check_apk(&root);
        assert_eq!(
            p.len(),
            1,
            "{p:?}",
            p = p.iter().map(|x| &x.0).collect::<Vec<_>>()
        );
        assert!(
            p[0].0.contains("ninguna de aarch64") && p[0].0.contains("UNTRUSTED"),
            "{}",
            p[0].0
        );
    }

    /// Y lo que no es una clave no cuenta como clave: un README en
    /// `/etc/apk/keys` dejaba pasar el contador de entradas.
    #[test]
    fn un_readme_no_cuenta_como_clave_de_apk() {
        let r = LinuxRootfs::new(Arch::X86_64);
        let root = rootfs_sana("claves-readme", Arch::X86_64, 62);
        let keys = root.join("etc").join("apk").join("keys");
        fs::remove_file(keys.join(clave_del_arco(Arch::X86_64))).unwrap();
        fs::write(keys.join("README.md"), b"las claves van aqui").unwrap();
        let p = r.check_apk(&root);
        assert_eq!(p.len(), 1);
        assert!(p[0].0.contains("UNTRUSTED"), "{}", p[0].0);
    }

    /// El firmware de la GSP es un ELF de RISC-V y vive en `/lib/firmware/`,
    /// y lo instala `image()` DESPUES de verificar. Sin la excepcion, la
    /// SEGUNDA construccion de una imagen de x86_64 --- que preserva la rootfs
    /// --- se encontraria el firmware de la pasada anterior y fallaria sola.
    #[test]
    fn el_firmware_no_es_userspace_del_objetivo() {
        let r = LinuxRootfs::new(Arch::X86_64);
        let root = rootfs_sana("firmware", Arch::X86_64, 62);
        let gsp = root.join("lib").join("firmware").join("nvidia").join("gsp");
        fs::create_dir_all(&gsp).unwrap();
        fs::write(gsp.join("gsp.bin"), elf64(EM_RISCV)).unwrap();
        let p = r.check_elf_machines(&root);
        assert!(
            p.is_empty(),
            "{:?}",
            p.iter().map(|x| &x.0).collect::<Vec<_>>()
        );

        // Pero un ELF de otro arco en `lib/` a secas si sale: la excepcion es
        // `lib/firmware`, no `lib` entero.
        fs::write(root.join("lib").join("libajena.so"), elf64(EM_RISCV)).unwrap();
        let p = r.check_elf_machines(&root);
        assert_eq!(
            p.len(),
            1,
            "{:?}",
            p.iter().map(|x| &x.0).collect::<Vec<_>>()
        );
        assert!(p[0].0.contains("libajena.so"), "{}", p[0].0);
    }

    /// RV32 y RV64 comparten `EM_RISCV`, asi que mirando solo la maquina un
    /// binario de 32 bits se colaba en una rootfs de riscv64.
    #[test]
    fn un_elf32_no_pasa_por_bueno() {
        let mut v = elf64(EM_RISCV);
        v[4] = 1; // ELFCLASS32
        let p = scratch("rv32.elf", &v);
        assert_eq!(elf_machine(&p), Some(MACHINE_RARA));
        assert_ne!(elf_machine(&p), Some(expected_machine(Arch::Riscv64)));
        // Y tampoco vale como cache de riscv64.
        assert!(!LinuxRootfs::new(Arch::Riscv64).cached_for_target(&p));
    }

    /// Solo la cabecera: con 20 bytes exactos ya se lee la maquina, asi que el
    /// barrido no carga en memoria el firmware ni los assets de la rootfs.
    #[test]
    fn solo_se_leen_los_veinte_primeros_bytes() {
        let mut v = elf64(EM_AARCH64);
        v.truncate(20);
        assert_eq!(elf_machine(&scratch("justo20.elf", &v)), Some(EM_AARCH64));
        v.truncate(19);
        assert_eq!(elf_machine(&scratch("corto19.elf", &v)), None);
    }

    /// Y el que cierra el circulo: un binario del host dentro de una rootfs
    /// cruzada. Es como se enlazaron las herramientas de `tools/` con el `ld`
    /// del anfitrion.
    #[test]
    fn un_binario_del_host_en_una_rootfs_cruzada_se_acusa() {
        let r = LinuxRootfs::with_variant(Arch::Aarch64, crate::variant::Variant::Desktop);
        let root = rootfs_sana("cruzada", Arch::Aarch64, 183);
        // El init esta bien; lo que se cuela es una herramienta del host.
        fs::create_dir_all(root.join("usr").join("bin")).unwrap();
        fs::write(root.join("usr").join("bin").join("lunarbar"), elf64(62)).unwrap();
        let p = r.check_elf_machines(&root);
        assert_eq!(
            p.len(),
            1,
            "{:?}",
            p.iter().map(|x| &x.0).collect::<Vec<_>>()
        );
        assert!(
            p[0].0.contains("lunarbar") && p[0].0.contains("x86_64"),
            "{}",
            p[0].0
        );
    }

    /// Una rootfs sin un solo ELF no es una rootfs: es un arbol de ficheros de
    /// configuracion, y pasaria las otras dos comprobaciones.
    #[test]
    fn una_rootfs_sin_ningun_elf_se_acusa() {
        let r = LinuxRootfs::new(Arch::X86_64);
        let root = std::env::temp_dir()
            .join("eclipse-verify-rootfs")
            .join("vacia");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("etc")).unwrap();
        fs::write(root.join("etc").join("hosts"), b"127.0.0.1 localhost\n").unwrap();
        let p = r.check_elf_machines(&root);
        assert_eq!(p.len(), 1);
        assert!(p[0].0.contains("no hay un solo ELF"), "{}", p[0].0);
    }

    /// Los enlaces no se siguen. Un rootfs esta lleno de ellos y sus destinos
    /// se resuelven DENTRO del sistema arrancado, no desde aqui: seguirlos da
    /// vueltas, cuenta el mismo ELF dos veces y, en un enlace absoluto o
    /// colgado, sale del arbol o revienta la lectura.
    #[test]
    fn los_enlaces_no_se_siguen() {
        let r = LinuxRootfs::with_variant(Arch::Aarch64, crate::variant::Variant::Desktop);
        let root = rootfs_sana("enlaces", Arch::Aarch64, 183);
        let bin = root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        // Un enlace colgado: seguirlo seria un error de lectura.
        #[cfg(unix)]
        std::os::unix::fs::symlink("no-existe", bin.join("colgado")).unwrap();
        // Y un enlace absoluto al ELF del host que ejecuta los tests: dentro
        // del sistema arrancado esa ruta es otra cosa, asi que mirarla aqui es
        // mirar la maquina equivocada.
        let fuera = root.parent().unwrap().join("binario-del-host");
        fs::write(&fuera, elf64(62)).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&fuera, bin.join("absoluto")).unwrap();

        assert!(
            r.check_elf_machines(&root).is_empty(),
            "un enlace no es un binario nuestro"
        );
    }

    /// Las suites traidas de fuera no son nuestras y no se miran: si se
    /// mirasen, un `libc-test` del host enrojeceria cada construccion cruzada
    /// y la comprobacion se apagaria por inservible.
    #[test]
    fn las_suites_ajenas_no_se_miran() {
        let r = LinuxRootfs::with_variant(Arch::Aarch64, crate::variant::Variant::Desktop);
        let root = rootfs_sana("suites", Arch::Aarch64, 183);
        for suite in ["libc-test", "other-test"] {
            let d = root.join(suite);
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("ajeno"), elf64(62)).unwrap();
        }
        assert!(r.check_elf_machines(&root).is_empty());
    }

    #[test]
    fn cada_arco_lleva_el_e_machine_de_elf_h() {
        assert_eq!(expected_machine(Arch::X86_64), 62);
        assert_eq!(expected_machine(Arch::Aarch64), 183);
        assert_eq!(expected_machine(Arch::Riscv64), 243);
        assert_eq!(machine_name(243), "riscv");
        assert_eq!(machine_name(62), "x86_64");
        assert_eq!(machine_name(183), "aarch64");
        assert_eq!(machine_name(0), "desconocida");
    }
}

/// Las dos redes de este arreglo viven en sitios que un refactor puede dejar
/// sin llamante sin que nada se queje: la llamada a `verify()` dentro de
/// `image()`, y el interruptor `MEM_DEBUG` del Makefile de zCore. Si
/// desaparecen, no falla nada --- simplemente volvemos a construir imagenes a
/// ciegas. Asi que se fijan aqui.
#[cfg(test)]
mod cableado_tests {
    use std::process::Command;

    /// El valor que `make` resuelve DE VERDAD para una variable, con los
    /// `ifeq` ya aplicados. Mas fuerte que grepear el fichero: un `ifeq` mal
    /// anidado grepea igual de bien y no añade la feature.
    fn zcore_features(extra: &[&str]) -> String {
        let out = Command::new("make")
            .current_dir(crate::PROJECT_DIR.join("zCore"))
            .arg("--no-print-directory")
            .arg("--eval=print-%: ; @echo $($*)")
            .arg("print-features")
            .args(extra)
            .output()
            .expect("make no se ha podido ejecutar");
        assert!(
            out.status.success(),
            "make print-features {extra:?} fallo: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// `MEM_DEBUG=1` tiene que llegar a la feature `mem-debug`, y no estar
    /// puesta cuando no se pide. Esta feature estuvo escrita y sin interruptor
    /// desde que nacio: el agujero que esto tapa.
    #[test]
    fn mem_debug_enciende_la_feature_y_solo_cuando_se_pide() {
        // Sin pedirla no aparece, en NINGUN arco: la puerta de arquitectura de
        // abajo esta dentro del `ifeq`, asi que un build normal de aarch64 o
        // riscv64 no puede tropezar con su `$(error)`.
        for arch in ["x86_64", "aarch64", "riscv64"] {
            let sin = zcore_features(&[&format!("ARCH={arch}")]);
            assert!(
                !sin.split_whitespace().any(|f| f == "mem-debug"),
                "mem-debug no se pidio y aparece en {arch}: {sin}"
            );
        }
        let con = zcore_features(&["ARCH=x86_64", "MEM_DEBUG=1"]);
        assert!(
            con.split_whitespace().any(|f| f == "mem-debug"),
            "MEM_DEBUG=1 no llego a la feature: {con}"
        );
    }

    /// Y fuera de x86_64 tiene que ser un error, no un silencio: las sondas
    /// viven en `memory_x86_64.rs`, asi que pedirla en otro arco daria una
    /// imagen sin depuracion de memoria y con cara de tenerla.
    #[test]
    fn mem_debug_fuera_de_x86_64_es_un_error() {
        for arch in ["aarch64", "riscv64"] {
            let out = Command::new("make")
                .current_dir(crate::PROJECT_DIR.join("zCore"))
                .arg("--no-print-directory")
                .arg("--eval=print-%: ; @echo $($*)")
                .arg("print-features")
                .args([&format!("ARCH={arch}"), "MEM_DEBUG=1"])
                .output()
                .expect("make no se ha podido ejecutar");
            assert!(
                !out.status.success(),
                "MEM_DEBUG=1 ARCH={arch} tenia que fallar y salio bien: {}",
                String::from_utf8_lossy(&out.stdout)
            );
            let err = String::from_utf8_lossy(&out.stderr);
            assert!(
                err.contains("MEM_DEBUG"),
                "el error no nombra MEM_DEBUG: {err}"
            );
        }
    }

    /// Las 21 caches de artefactos de `mod.rs` tienen que seguir pasando por
    /// `cached_for_target`. Perder una no falla nada: simplemente esa
    /// herramienta vuelve a poder llegar a la rootfs con el arco del build
    /// anterior, que es el fallo que vino a tapar. Y el autor de la numero 22
    /// no tiene por que saber que la puerta existe, asi que la cuenta se
    /// comprueba aqui en vez de confiar en que se acuerde.
    #[test]
    fn toda_cache_de_artefacto_pasa_por_la_puerta_del_arco() {
        let src = std::fs::read_to_string(
            crate::PROJECT_DIR
                .join("xtask")
                .join("src")
                .join("linux")
                .join("mod.rs"),
        )
        .unwrap();

        // Las que comparan contra un `source` suelto.
        let mut sin_puerta = Vec::new();
        for (n, l) in src.lines().enumerate() {
            let t = l.trim();
            let es_puerta = t.starts_with("if ")
                && t.contains(".is_file() && source.is_file()")
                && t.ends_with('{');
            if es_puerta && !t.contains("cached_for_target") {
                sin_puerta.push(n + 1);
            }
        }
        assert!(
            sin_puerta.is_empty(),
            "caches sin comprobar el arco, en las lineas {sin_puerta:?} de mod.rs: \
             añade `&& self.cached_for_target(&<artefacto>)` a la condicion"
        );

        // Y la cuenta, para que quitar una puerta se note aunque su `if`
        // cambie de forma. Si añades una cache nueva --con su puerta-- sube
        // este numero.
        let puertas = src.matches("self.cached_for_target(").count();
        assert_eq!(
            puertas, 21,
            "las puertas del arco eran 21 y ahora son {puertas}: si has añadido \
             una cache con la suya, sube el numero; si has quitado una puerta, no"
        );
    }

    /// `make image` tiene que verificar la rootfs sin que nadie se acuerde de
    /// pedirlo. Si esta llamada se cae, la comprobacion sigue existiendo y ya
    /// no corre: exactamente la forma del fallo que vino a tapar.
    #[test]
    fn image_verifica_la_rootfs_que_acaba_de_construir() {
        let src = std::fs::read_to_string(
            crate::PROJECT_DIR
                .join("xtask")
                .join("src")
                .join("linux")
                .join("image.rs"),
        )
        .unwrap();
        let cuerpo = src
            .split_once("pub fn image(&self) {")
            .expect("image() ya no se llama asi")
            .1;
        // Por LINEAS y descartando comentarios: `// self.verify();` contiene
        // el mismo texto, y comentar la llamada es justo como se pierde una
        // red de seguridad sin que nada se queje.
        let viva = |aguja: &str| {
            cuerpo.lines().position(|l| {
                let l = l.trim_start();
                !l.starts_with("//") && l.contains(aguja)
            })
        };
        let make =
            viva("self.make(false);").expect("image() ya no construye la rootfs con make(false)");
        let verify = viva("self.verify();")
            .expect("image() ya no verifica la rootfs: la red de seguridad esta sin llamante");
        assert!(
            verify > make,
            "verify() tiene que ir DESPUES de construir la rootfs, no antes"
        );
    }
}
