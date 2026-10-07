//! Las dos variantes de imagen que `make release` publica.
//!
//! `desktop` es la Eclipse de siempre: la sesión labwc (o Xorg) con todo el
//! cierre de paquetes que `xorg::install` baja a `apk` — Mesa, libLLVM, fuentes,
//! Firefox, XFCE. `minimal` es el mismo sistema base sin nada de eso: consola,
//! red, audio y `install-eclipse`, y `/etc/eclipse/desktop` puesto a `none` para
//! que `eclipse-init` no arranque compositor alguno.
//!
//! Son dos **rootfs distintos** (`rootfs/<arch>` y `rootfs/<arch>-minimal`), no
//! un recorte posterior de uno solo: así la variante minimal nunca tiene en
//! disco los ficheros del escritorio, y no hace falta mantener una lista de qué
//! podar — algo que se desincroniza en silencio cada vez que se añade un
//! paquete. Cuesta un segundo `apk add` (cacheado) y un segundo busybox, que es
//! lo que vale una variante que de verdad no lleva el escritorio.
//!
//! El sufijo de `desktop` está VACÍO a propósito: `rootfs/x86_64`,
//! `ignored/target/efi.img.gz`, `zCore/x86_64.img`… siguen donde estaban, así
//! que `make qemu`, `make image` y todo script que mire esas rutas se comportan
//! exactamente como antes de que existiera este eje.

use crate::XError;
use std::str::FromStr;

/// Qué variante de imagen se está construyendo.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Variant {
    /// Eclipse con escritorio (labwc/Xorg + el cierre de apk). El de siempre.
    Desktop,
    /// Eclipse de consola: sin escritorio, sin Mesa, sin Firefox.
    Minimal,
}

impl Variant {
    /// El nombre con el que se pide por la línea de órdenes y con el que sale
    /// en el nombre de la ISO.
    #[inline]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Desktop => "desktop",
            Self::Minimal => "minimal",
        }
    }

    /// Lo que se le añade a cada ruta de artefacto intermedio. Vacío para
    /// `desktop`, que es lo que mantiene las rutas históricas intactas.
    #[inline]
    pub const fn suffix(&self) -> &'static str {
        match self {
            Self::Desktop => "",
            Self::Minimal => "-minimal",
        }
    }

    /// La sesión que va en `/etc/eclipse/desktop`, el valor persistente que
    /// `eclipse-init` lee cuando el cmdline no trae `desktop=`.
    ///
    /// `none` no es «sin preferencia»: es un valor que la tabla de servicios
    /// entiende, y con el que descarta toda fila con `desktop =`. Sin él, una
    /// imagen minimal arrancaría buscando labwc y dejaría a un servicio
    /// reintentando un binario que no está instalado.
    #[inline]
    pub const fn default_session(&self) -> &'static str {
        match self {
            Self::Desktop => "labwc",
            Self::Minimal => "none",
        }
    }

    /// Si esta variante lleva escritorio. Lo consultan tanto el montaje del
    /// rootfs (para no llamar a `desktop::install`/`xorg::install`) como la
    /// construcción de la imagen (para no copiar los árboles de X al root vivo).
    #[inline]
    pub const fn has_desktop(&self) -> bool {
        matches!(self, Self::Desktop)
    }
}

impl FromStr for Variant {
    type Err = XError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_lowercase().as_str() {
            // `full` y `console` porque son las dos palabras que uno teclea sin
            // mirar la ayuda, y rechazarlas no enseña nada a nadie.
            "desktop" | "full" => Ok(Self::Desktop),
            "minimal" | "min" | "console" => Ok(Self::Minimal),
            _ => Err(XError::EnumParse {
                type_name: "Variant",
                value: s.into(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// El sufijo de `desktop` tiene que seguir siendo vacío: es lo único que
    /// garantiza que `rootfs/x86_64`, `zCore/x86_64.img` y
    /// `ignored/target/efi.img.gz` sigan siendo las rutas de siempre. Un
    /// sufijo aquí (`-desktop`) dejaría `make qemu` apuntando a una imagen que
    /// ya nadie escribe, y lo haría en silencio.
    #[test]
    fn la_variante_de_escritorio_no_renombra_nada() {
        assert_eq!(Variant::Desktop.suffix(), "");
        assert_eq!(Variant::Minimal.suffix(), "-minimal");
    }

    /// `/etc/eclipse/desktop` lo escribe `install_eclipse_init`, y el valor
    /// sale de aquí. `labwc` en la minimal sería un arranque buscando un
    /// compositor que la imagen no lleva; `none` en la de escritorio sería una
    /// Eclipse de siempre que arranca a consola. Las dos equivocaciones son
    /// invisibles hasta que alguien arranca la imagen.
    #[test]
    fn cada_variante_tiene_su_sesion_por_defecto() {
        assert_eq!(Variant::Desktop.default_session(), "labwc");
        assert_eq!(Variant::Minimal.default_session(), "none");
    }

    #[test]
    fn solo_la_de_escritorio_lleva_escritorio() {
        assert!(Variant::Desktop.has_desktop());
        assert!(!Variant::Minimal.has_desktop());
    }

    #[test]
    fn se_parsea_por_nombre_y_por_los_sinonimos_obvios() {
        for s in ["desktop", "Desktop", " DESKTOP ", "full"] {
            assert_eq!(s.parse::<Variant>().unwrap(), Variant::Desktop, "{s:?}");
        }
        for s in ["minimal", "MIN", "console"] {
            assert_eq!(s.parse::<Variant>().unwrap(), Variant::Minimal, "{s:?}");
        }
        assert!("escritorio".parse::<Variant>().is_err());
        assert!("".parse::<Variant>().is_err());
    }

    /// El nombre que imprime el build y el que lleva la ISO tiene que volver a
    /// parsearse a la misma variante: es el contrato entre `make release`
    /// (que pasa `VARIANT=`) y el xtask.
    #[test]
    fn el_nombre_de_cada_variante_vuelve_a_parsearse_a_ella_misma() {
        for v in [Variant::Desktop, Variant::Minimal] {
            assert_eq!(v.name().parse::<Variant>().unwrap(), v, "{v:?}");
        }
        assert_ne!(Variant::Desktop.name(), Variant::Minimal.name());
    }
}

/// Lo que la variante tiene que atravesar FUERA del xtask: los dos Makefiles y
/// el script que dimensiona el ESP.
///
/// Estos tests existen porque los cuatro fallos que encontró la revisión del
/// #1754 estaban todos ahí: el eje de variantes llegaba a `cargo image` y se
/// paraba, así que `make qemu VARIANT=minimal` construía la imagen minimal y
/// arrancaba la de escritorio. Un test que mire el texto del Makefile no sirve
/// (un `ifeq` sin resolver no dice qué se ejecuta), así que se le pregunta a
/// make: `-p -q` imprime su propia base de datos ya evaluada, sin construir
/// nada, y `--eval` añade un objetivo que imprime una variable.
#[cfg(test)]
mod build_wiring_tests {
    use crate::PROJECT_DIR;
    use std::process::Command;

    /// El valor que make calcula para `var`, con `VARIANT=variant`.
    fn make_var(dir: &str, var: &str, extra: &[&str]) -> String {
        let out = Command::new("make")
            .current_dir(PROJECT_DIR.join(dir))
            .arg("--no-print-directory")
            .arg("--eval=print-%: ; @echo $($*)")
            .arg(format!("print-{var}"))
            .args(extra)
            .output()
            .expect("make no se ha podido ejecutar");
        assert!(
            out.status.success(),
            "make print-{var} {extra:?} falló: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// La receta que make EJECUTARÍA para `target`, tal como la tiene parseada
    /// (los `ifeq` ya resueltos), sin ejecutar nada.
    fn recipe_of(target: &str) -> String {
        let out = Command::new("make")
            .current_dir(*PROJECT_DIR)
            .args(["-p", "-q"])
            .output()
            .expect("make -p no se ha podido ejecutar");
        let db = String::from_utf8_lossy(&out.stdout);
        let start = db
            .find(&format!("\n{target}:"))
            .unwrap_or_else(|| panic!("{target} no aparece en la base de datos de make"));
        let rest = &db[start + 1..];
        let end = rest.find("\n\n").unwrap_or(rest.len());
        rest[..end].to_string()
    }

    /// El fallo que importa de los cuatro: `make qemu VARIANT=minimal`
    /// construía `zCore/x86_64-minimal.img` y acto seguido arrancaba
    /// `zCore/x86_64.img`, la de escritorio. Si `user_img` pierde el sufijo,
    /// vuelve a pasar, y en silencio.
    #[test]
    fn el_makefile_de_zcore_arranca_la_imagen_de_su_variante() {
        let common = ["ARCH=x86_64", "LINUX=1"];
        for (variant, want) in [("desktop", "x86_64.img"), ("minimal", "x86_64-minimal.img")] {
            let args: Vec<&str> = common
                .iter()
                .copied()
                .chain([match variant {
                    "desktop" => "VARIANT=desktop",
                    _ => "VARIANT=minimal",
                }])
                .collect();
            assert_eq!(make_var("zCore", "user_img", &args), want, "{variant}");
            // INITRAMFS_IMG cae a user_img por defecto, y es lo que acaba en
            // el ESP como initramfs.img.
            assert_eq!(
                make_var("zCore", "INITRAMFS_IMG", &args),
                want,
                "{variant}: INITRAMFS_IMG"
            );
        }
    }

    /// `desktop=` en el cmdline GANA sobre `/etc/eclipse/desktop`, así que un
    /// `DESKTOP=labwc` fijo le decía a una imagen sin compositor que arrancara
    /// labwc, pisando el `none` que su propio rootfs había escrito.
    #[test]
    fn la_sesion_del_cmdline_sigue_a_la_variante() {
        assert_eq!(make_var(".", "DESKTOP", &["VARIANT=desktop"]), "labwc");
        assert_eq!(make_var(".", "DESKTOP", &["VARIANT=minimal"]), "none");
        // Y se puede seguir forzando a mano, que es para lo que está el knob.
        assert_eq!(
            make_var(".", "DESKTOP", &["VARIANT=minimal", "DESKTOP=xorg"]),
            "xorg"
        );
    }

    /// Las tres salidas de `dist/` llevan la variante en el nombre, así que
    /// las tres tienen que construir el ESP de ESA variante. `qcow2` e `img`
    /// empaquetaban `target/<arch>/release/esp`, que no tiene variante en la
    /// ruta, tal como se lo encontraran: `make iso VARIANT=desktop` seguido de
    /// `make qcow2 VARIANT=minimal` metía el instalador de escritorio dentro
    /// de un fichero llamado minimal, sin decir nada.
    #[test]
    fn los_tres_objetivos_de_distribucion_construyen_el_esp_de_su_variante() {
        for target in ["iso", "qcow2", "img"] {
            let recipe = recipe_of(target);
            assert!(
                recipe.contains("esp-for-variant"),
                "{target} no prepara el ESP de la variante:\n{recipe}"
            );
            assert!(
                recipe.contains("VARIANT=$(VARIANT)"),
                "{target} prepara el ESP sin pasarle la variante:\n{recipe}"
            );
        }
    }

    /// Los lanzadores de VM tienen que bajarle la variante a zCore; si no,
    /// construyen una imagen y arrancan la otra.
    #[test]
    fn los_lanzadores_le_pasan_la_variante_a_zcore() {
        for target in ["qemu", "vbox"] {
            let recipe = recipe_of(target);
            assert!(
                recipe.contains("VARIANT=$(VARIANT)"),
                "{target} no le pasa la variante a zCore:\n{recipe}"
            );
        }
    }

    /// Una variante inventada tiene que parar el build en los dos Makefiles,
    /// no caer a `desktop` por descuido.
    #[test]
    fn una_variante_que_no_existe_para_el_build() {
        for dir in [".", "zCore"] {
            let out = Command::new("make")
                .current_dir(PROJECT_DIR.join(dir))
                .args(["--no-print-directory", "-p", "-q", "VARIANT=escritorio"])
                .output()
                .expect("make no se ha podido ejecutar");
            assert!(
                !out.status.success(),
                "{dir}: una variante inventada no ha parado el build"
            );
            assert!(
                String::from_utf8_lossy(&out.stderr).contains("VARIANT debe ser"),
                "{dir}: ha fallado, pero no por la variante"
            );
        }
    }
}

/// `scripts/esp-size-mb.sh`: la medida de la que sale el tamaño del ESP.
///
/// Vive aquí porque es la otra mitad del mismo encargo: los nombres de
/// `dist/` ya dicen la variante, y una FAT32 corta se lee en la consola del
/// invitado como un kernel colgado, no como un ESP sin `BootX64.efi`.
#[cfg(test)]
mod esp_size_tests {
    use crate::PROJECT_DIR;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "eclipse-espsize-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn script() -> PathBuf {
        PROJECT_DIR.join("scripts").join("esp-size-mb.sh")
    }

    /// Un fichero DISPERSO de `mb` MiB: longitud declarada y cero bloques,
    /// que es exactamente como `fuse` escribe la SFS del initramfs.
    fn sparse(dir: &Path, name: &str, mb: u64) {
        let f = fs::File::create(dir.join(name)).unwrap();
        f.set_len(mb * 1024 * 1024).unwrap();
    }

    fn run(dir: &Path, headroom: &str, path_prefix: Option<&Path>) -> (bool, String, String) {
        let mut cmd = Command::new("sh");
        cmd.arg(script()).arg(dir).arg(headroom);
        if let Some(p) = path_prefix {
            let old = std::env::var("PATH").unwrap_or_default();
            cmd.env("PATH", format!("{}:{old}", p.display()));
        }
        let out = cmd.output().expect("sh no se ha podido ejecutar");
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        )
    }

    /// Un `du` que rechaza `--apparent-size`, como el de macOS o el de
    /// busybox: es el único caso en el que la rama de reserva se ejecuta.
    /// OJO: el directorio devuelto no puede estar DENTRO del arbol que se
    /// mide, o el propio `du` falso entra en la suma (costó un 297 contra un
    /// 296 al escribir estos tests).
    fn du_without_apparent_size(tag: &str) -> PathBuf {
        let bin = scratch(tag);
        let du = bin.join("du");
        fs::write(
            &du,
            "#!/bin/sh\n\
             for a in \"$@\"; do\n\
             \x20 [ \"$a\" = \"--apparent-size\" ] && { echo 'du: illegal option' >&2; exit 1; }\n\
             done\n\
             exec /usr/bin/du \"$@\"\n",
        )
        .unwrap();
        fs::set_permissions(&du, fs::Permissions::from_mode(0o755)).unwrap();
        bin
    }

    /// La medida es el tamaño APARENTE. Con bloques, un initramfs disperso de
    /// 200 MiB cuenta como 1, la FAT32 sale corta y `mcopy` muere en
    /// `BootX64.efi`, que va el último: una ISO sin con qué arrancar.
    #[test]
    fn mide_el_tamano_aparente_no_los_bloques() {
        let dir = scratch("aparente");
        sparse(&dir, "initramfs.img", 200);

        let (ok, out, err) = run(&dir, "96", None);
        assert!(ok, "el script ha fallado: {err}");
        assert_eq!(out, "296", "200 MiB aparentes + 96 de holgura");

        // Y que el caso es de verdad el que dice: en bloques son ~0.
        let blocks = Command::new("du")
            .args(["-sm".as_ref(), dir.as_os_str()])
            .output()
            .unwrap();
        let blocks: u64 = String::from_utf8_lossy(&blocks.stdout)
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert!(
            blocks < 10,
            "el fichero no ha salido disperso ({blocks} MiB)"
        );
    }

    /// EL test de este arreglo. La forma anterior era
    ///
    ///     esp_mb=$(du -sm --apparent-size "$d" 2>/dev/null | cut -f1 \
    ///              || du -sm "$d" | cut -f1)
    ///
    /// y `||` se ata a la TUBERÍA entera, cuyo estado es el del ÚLTIMO
    /// mandato. Con un `du` sin `--apparent-size`: el `du` falla, su stderr se
    /// traga, `cut` lee EOF y sale 0, la tubería «funciona» con salida vacía y
    /// la reserva NO se ejecuta nunca. `esp_mb` queda vacío, `$((esp_mb + 96))`
    /// es 96, y el ESP sale de 96 MiB: el fallo que `--apparent-size` existía
    /// para evitar, devuelto por su propia red de seguridad.
    #[test]
    fn sin_apparent_size_la_reserva_mide_igual_y_no_cae_a_la_holgura() {
        let dir = scratch("reserva");
        sparse(&dir, "initramfs.img", 200);
        let fake = du_without_apparent_size("reserva-fakebin");

        let (ok, out, err) = run(&dir, "96", Some(&fake));
        assert!(ok, "el script ha fallado con un du sin la opción: {err}");
        // Lo que se afirma es que la reserva MIDE, no que empate al byte con
        // GNU du: con 96 MiB de holgura un MiB de diferencia no es nada, y un
        // 96 clavado es la señal de que no se ejecutó.
        let mb: u64 = out.parse().unwrap_or_else(|_| panic!("salida {out:?}"));
        assert!(
            (296..=297).contains(&mb),
            "la reserva no ha medido el árbol: {mb} MiB (esperado ~296)"
        );
        assert_ne!(mb, 96, "ha caído a la holgura: la reserva no se ejecutó");
    }

    /// Un árbol que no se puede medir es un error con nombre, no un 0 que
    /// luego es un ESP de 96 MiB.
    #[test]
    fn un_arbol_que_no_se_puede_medir_es_un_error() {
        let dir = scratch("inexistente");
        let (ok, out, err) = run(&dir.join("no-existe"), "96", None);
        assert!(!ok, "un directorio que no existe ha devuelto {out:?}");
        assert!(err.contains("no existe el directorio"), "stderr: {err}");
    }

    /// Redondea hacia ARRIBA: un árbol de menos de 1 MiB necesita 1, no 0.
    #[test]
    fn redondea_hacia_arriba() {
        let dir = scratch("redondeo");
        fs::write(dir.join("f"), b"hola").unwrap();
        let fake = du_without_apparent_size("redondeo-fakebin");
        assert_eq!(run(&dir, "0", Some(&fake)).1, "1");
    }
}
