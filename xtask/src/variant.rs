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
