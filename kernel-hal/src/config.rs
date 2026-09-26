use crate::utils::init_once::InitOnce;

pub use super::imp::config::KernelConfig;

#[cfg(feature = "libos")]
pub(crate) static KCONFIG: InitOnce<KernelConfig> = InitOnce::new_with_default(KernelConfig);

#[cfg(not(feature = "libos"))]
pub(crate) static KCONFIG: InitOnce<KernelConfig> = InitOnce::new();

/// Número máximo de CPUs (id lógico denso 0..MAX_CORE_NUM).
///
/// Debe ser <= al `MAX_CORE_NUM` interno del crate `lock` (vendor/kernel-sync),
/// que dimensiona su array per-CPU indexado por el mismo id lógico.
pub const MAX_CORE_NUM: usize = 64;

/// Y eso de arriba era una frase, no una comprobación. Las dos constantes valen
/// 64 hoy, así que nada lo delataba; el día que una de las dos se mueva sin la
/// otra, `lock` reparte un id lógico que las tablas per-CPU de este crate no
/// pueden indexar, y **todas** sus guardas están escritas `< MAX_CORE_NUM`: no
/// hay un panic ni un aviso, hay una CPU que deja de recibir IPIs y cuyos
/// acuses de TLB shootdown no llega a dar nunca. Eso es exactamente el fallo
/// que `common::ipi` está entero construido para no tener, y el único sitio
/// donde se puede impedir es aquí, antes de compilar.
const _: () = assert!(
    MAX_CORE_NUM <= lock::MAX_CORE_NUM,
    "kernel_hal::config::MAX_CORE_NUM es mayor que el de `lock`: `lock` repartiría \
     ids lógicos que las tablas per-CPU de kernel-hal rechazan en silencio"
);
