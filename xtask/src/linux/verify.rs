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
use crate::LinuxRootfs;
use std::{fs, path::Path};

/// La escotilla, por si alguien quiere a propósito una imagen incompleta.
const ALLOW_INCOMPLETE: &str = "ECLIPSE_ALLOW_INCOMPLETE_ROOTFS";

/// `e_machine` de la cabecera ELF que tiene que llevar todo binario propio de
/// la rootfs. Los valores son los de `elf.h`.
const EM_X86_64: u16 = 62;
const EM_AARCH64: u16 = 183;
const EM_RISCV: u16 = 243;

/// Lo que cada binario de la rootfs tiene que declarar para este arco.
const fn expected_machine(arch: Arch) -> u16 {
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
        _ => "desconocida",
    }
}

/// Lee el `e_machine` de un fichero, o `None` si no es un ELF.
///
/// Solo mira los 20 primeros bytes: el número mágico, la clase (que tiene que
/// ser ELF64 en los tres arcos que soportamos) y la máquina. Un script, una
/// imagen o un fichero de texto no son un ELF y se saltan sin ruido --- no es
/// un error que `/etc/profile` no tenga cabecera.
fn elf_machine(path: &Path) -> Option<u16> {
    let bytes = fs::read(path).ok()?;
    if bytes.len() < 20 || &bytes[..4] != b"\x7fELF" {
        return None;
    }
    // EI_DATA: 1 = little endian, que es lo único que construimos. Un ELF big
    // endian aquí ya sería la anomalía que buscamos, así que no se salta: se
    // devuelve una máquina que no va a coincidir con ninguna esperada.
    if bytes[5] != 1 {
        return Some(u16::MAX);
    }
    Some(u16::from_le_bytes([bytes[18], bytes[19]]))
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
        let n = fs::read_dir(&keys)
            .map(|d| d.filter_map(Result::ok).count())
            .unwrap_or(0);
        if n == 0 {
            out.push(Problem(
                "/etc/apk/keys esta vacio: apk dice UNTRUSTED signature y no instala nada, \
                 en silencio"
                    .into(),
            ));
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
        fs::write(apk.join("keys").join("una.rsa.pub"), b"clave").unwrap();
        root
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
                .join("una.rsa.pub"),
        )
        .unwrap();
        let p = r.check_apk(&root);
        assert_eq!(p.len(), 1);
        assert!(p[0].0.contains("UNTRUSTED"), "{}", p[0].0);
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
