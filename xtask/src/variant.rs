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
        recipe_of_with(target, &[])
    }

    /// Lo mismo, con variables en la línea de órdenes. Hace falta para los
    /// objetivos que se bifurcan por `ARCH`: la receta de `iso` depende de
    /// cuál sea, y sin esto el test solo veía la del arch del host.
    fn recipe_of_with(target: &str, extra: &[&str]) -> String {
        let out = Command::new("make")
            .current_dir(*PROJECT_DIR)
            .args(["-p", "-q"])
            .args(extra)
            .output()
            .expect("make -p no se ha podido ejecutar");
        let db = String::from_utf8_lossy(&out.stdout);
        let start = db
            .find(&format!("\n{target}:"))
            .unwrap_or_else(|| panic!("{target} no aparece en la base de datos de make"));
        let rest = &db[start + 1..];
        // Hasta la siguiente ENTRADA de la base de datos, no hasta el primer
        // hueco: una receta puede llevar lineas vacias dentro (un `@echo ""`),
        // y cortar ahi dejaba fuera la mitad de la de `iso` -- con lo que un
        // test podia pasar por no haber llegado a leer la linea que juzga.
        let mut out = Vec::new();
        for (i, line) in rest.lines().enumerate() {
            let continues =
                i == 0 || line.is_empty() || line.starts_with('\t') || line.starts_with('#');
            if !continues {
                break;
            }
            out.push(line);
        }
        out.join("\n")
    }

    /// Lo que hace posible la ISO de arm64. Un medio no puede traer dos
    /// dispositivos de bloques, y el rayboot precompilado no sabe pasar un
    /// initramfs, asi que la imagen viaja DENTRO del kernel. Sin
    /// `LINK_USER_IMG=1` el kernel sale sin ella, arranca, y se queda
    /// buscando una raiz que en una ISO no existe.
    #[test]
    fn el_iso_de_arm64_empotra_la_imagen_en_el_kernel() {
        let recipe = recipe_of("iso-aarch64");
        assert!(
            recipe.contains("LINK_USER_IMG=1"),
            "la receta de iso-aarch64 ya no empotra la imagen:\n{recipe}"
        );
        // Y el knob tiene que estar apagado por defecto, o `make qemu
        // ARCH=aarch64` empieza a construir un kernel de 100+ MiB para nada.
        let common = ["ARCH=aarch64", "LINUX=1"];
        assert!(
            !make_var("zCore", "features", &common).contains("link-user-img"),
            "link-user-img se ha encendido por defecto"
        );
        let mut on: Vec<&str> = common.to_vec();
        on.push("LINK_USER_IMG=1");
        assert!(
            make_var("zCore", "features", &on).contains("link-user-img"),
            "LINK_USER_IMG=1 no enciende la feature"
        );
    }

    /// Y la imagen que se empotra tiene que ser LA DE SU VARIANTE. `build.rs`
    /// lee `USER_IMG` del entorno, y como ya imprime lineas
    /// `rerun-if-changed`, cargo deja de re-ejecutarlo cuando cambia una
    /// variable de entorno salvo que esté declarada. Sin la declaracion,
    /// construir las dos variantes seguidas --que es justo lo que hace
    /// `make release`-- reutiliza el `USER_IMG` de la PRIMERA: la ISO minimal
    /// saldria con el rootfs de escritorio dentro, y en silencio.
    #[test]
    fn el_build_script_se_reejecuta_si_cambia_la_imagen() {
        let build_rs = std::fs::read_to_string(PROJECT_DIR.join("zCore").join("build.rs"))
            .expect("no se ha podido leer zCore/build.rs");
        assert!(
            build_rs.contains("cargo:rerun-if-env-changed=USER_IMG"),
            "zCore/build.rs lee USER_IMG sin declarar que un cambio lo \
             re-ejecuta: las dos variantes compartirian imagen"
        );
    }

    /// rayboot lee el kernel de `os` en la RAIZ del ESP (`KERNEL_LOCATION` en
    /// su config), no de `/EFI/...`. Copiarlo a cualquier otro sitio da un
    /// arranque que muere antes de la primera linea del kernel.
    #[test]
    fn el_kernel_del_iso_de_arm64_va_en_la_raiz_del_esp() {
        let recipe = recipe_of("iso-aarch64");
        assert!(
            recipe.contains("::/os"),
            "la receta ya no copia el kernel a ::/os:\n{recipe}"
        );
    }

    /// La sesion de la ISO es consola, como la de x86_64: `desktop=none` tiene
    /// que estar en el cmdline del Boot.json y no solo en el build de zCore,
    /// porque el objetivo `build` no toca el Boot.json (solo `run` lo hace).
    #[test]
    fn el_iso_de_arm64_arranca_sin_escritorio() {
        let recipe = recipe_of("iso-aarch64");
        assert!(
            recipe.contains("DESKTOP=none"),
            "el kernel de la ISO se construye con escritorio:\n{recipe}"
        );
        let cmdline = make_var(".", "ISO_CMDLINE_AARCH64", &["ARCH=aarch64"]);
        assert!(
            cmdline.contains("desktop=none"),
            "{cmdline:?} no apaga el escritorio"
        );
    }

    /// Y NO puede nombrar un disco raiz. En x86_64 el cmdline lleva
    /// `ROOT=/dev/vda`; aqui la raiz es la imagen que va dentro del kernel, y
    /// un `ROOT=` apuntando a un disco que no esta manda al kernel a buscar
    /// algo que no existe.
    #[test]
    fn el_cmdline_del_iso_de_arm64_no_nombra_un_disco_raiz() {
        let cmdline = make_var(".", "ISO_CMDLINE_AARCH64", &["ARCH=aarch64"]);
        // `ROOTPROC=` lleva ROOT dentro, asi que se busca el valor, no la
        // palabra: lo que no puede aparecer es una raiz de bloques.
        assert!(
            !cmdline.contains("ROOT=/dev"),
            "{cmdline:?} nombra un disco raiz que en una ISO no existe"
        );
        assert!(
            cmdline.contains("ROOTPROC="),
            "{cmdline:?} se ha quedado sin proceso de consola"
        );
    }

    /// `iso` tiene que repartir por arquitectura: x86_64 y aarch64 construyen
    /// una, y lo demas cae en el objetivo que explica por que no.
    #[test]
    fn el_iso_reparte_por_arquitectura() {
        let arm = recipe_of_with("iso", &["ARCH=aarch64"]);
        assert!(
            arm.contains("iso-aarch64"),
            "iso con ARCH=aarch64 no llama a iso-aarch64:\n{arm}"
        );
        let risc = recipe_of_with("iso", &["ARCH=riscv64"]);
        assert!(
            risc.contains("iso-unsupported-arch"),
            "iso con ARCH=riscv64 no explica por que no hay ISO:\n{risc}"
        );
        let x86 = recipe_of_with("iso", &["ARCH=x86_64"]);
        assert!(
            x86.contains("xorriso") && !x86.contains("iso-unsupported-arch"),
            "iso con ARCH=x86_64 ha dejado de construir la ISO:\n{x86}"
        );
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

    /// Las tres recetas que miden el ESP tienen que conservar el fallo del
    /// script, no tragárselo. Ver
    /// `un_fallo_al_medir_para_la_receta_y_no_cae_al_minimo` para el porqué.
    #[test]
    fn las_recetas_conservan_el_fallo_de_la_medida() {
        // `iso-aarch64` mide su propio ESP igual que las tres de x86_64, asi
        // que entra en la misma regla: un fallo de la medida tiene que parar
        // la receta, no colarse como un tamaño cualquiera.
        for target in ["iso", "qcow2", "img", "iso-aarch64"] {
            let recipe = recipe_of(target);
            // Solo las lineas que la LLAMAN: el comentario de encima la
            // nombra tambien.
            let medida: Vec<&str> = recipe
                .lines()
                .filter(|l| l.contains("esp-size-mb.sh") && !l.trim_start().starts_with("@#"))
                .collect();
            assert_eq!(medida.len(), 1, "{target}: {recipe}");
            assert!(
                medida[0].contains("|| exit 1"),
                "{target} se traga un fallo de la medida:\n{}",
                medida[0]
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

    /// Nadie vuelve a tener su propia copia de esta cuenta.
    ///
    /// Esto ya pasó una vez: `zCore/Makefile` tenía el cálculo correcto y los
    /// tres scripts de `scripts/` nunca lo recibieron, así que arrancar por
    /// `qemu-bench.sh` daba una FAT32 corta mientras `make qemu` iba bien. Y
    /// volvió a pasar en esta tanda: arreglé los Makefiles y los scripts
    /// seguían con la forma vieja, dos con el `||` detrás de la tubería y el
    /// tercero cayendo a contar bloques. Un `du -sm` nuevo en `scripts/` es
    /// una cuarta copia esperando a desincronizarse.
    #[test]
    fn ningun_script_dimensiona_el_esp_por_su_cuenta() {
        let dir = PROJECT_DIR.join("scripts");
        let mut culpables = Vec::new();
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.file_name().and_then(|n| n.to_str()) == Some("esp-size-mb.sh") {
                continue;
            }
            let Ok(body) = fs::read_to_string(&path) else {
                continue;
            };
            for (n, line) in body.lines().enumerate() {
                // El comentario puede nombrar `du -sm` al explicar por qué no
                // se usa; lo que no puede es volver a calcularlo.
                let code = line.split('#').next().unwrap_or("");
                if code.contains("du -sm") || code.contains("du -h") {
                    culpables.push(format!("{}:{}", path.display(), n + 1));
                }
            }
        }
        assert!(
            culpables.is_empty(),
            "estos sitios vuelven a medir el ESP por su cuenta en vez de usar \
             scripts/esp-size-mb.sh: {culpables:?}"
        );
    }

    /// Un fallo del script tiene que PARAR la receta.
    ///
    /// En `qcow2` e `img` la medida va seguida de un mínimo:
    ///
    ///     esp_mb=$(esp-size-mb.sh ...); [ "$esp_mb" -ge 1024 ] || esp_mb=1024
    ///
    /// Sin `|| exit 1`, un fallo del script deja `esp_mb` vacío, el `-ge`
    /// falla por comparar una cadena vacía, se toma su rama `||`, y la receta
    /// sigue con 1024 MiB **y termina con éxito**: justo el error silencioso
    /// que el script existe para evitar. Aquí se compara una forma contra la
    /// otra con un script que falla siempre.
    #[test]
    fn un_fallo_al_medir_para_la_receta_y_no_cae_al_minimo() {
        let vieja = "esp_mb=$(exit 3); [ \"$esp_mb\" -ge 1024 ] || esp_mb=1024; echo $esp_mb";
        let nueva =
            "esp_mb=$(exit 3) || exit 1; [ \"$esp_mb\" -ge 1024 ] || esp_mb=1024; echo $esp_mb";

        let run = |script: &str| {
            let out = Command::new("sh").arg("-c").arg(script).output().unwrap();
            (
                out.status.success(),
                String::from_utf8_lossy(&out.stdout).trim().to_string(),
            )
        };

        // La forma vieja: éxito, y con el mínimo fijo puesto a dedo.
        assert_eq!(run(vieja), (true, "1024".to_string()));
        // La nueva: para, y no imprime tamaño alguno.
        let (ok, salida) = run(nueva);
        assert!(!ok, "la receta ha seguido adelante tras fallar la medida");
        assert_eq!(salida, "", "no debería haber llegado a imprimir un tamaño");
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
