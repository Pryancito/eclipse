//! El `mip` del BCM2712: el reparto de MSI del lado del puente de la Pi 5.
//!
//! El RP1 ([`super::rp1`]) es el lado del dispositivo: sus 61 líneas son 61
//! vectores MSI-X y su bloque de configuración decide cuál está encendida.
//! Esto es el otro extremo del mismo cable: lo que recibe esos mensajes y los
//! convierte en interrupciones del GIC. Sin él, la tabla MSI-X del RP1 apunta
//! a una dirección que no contesta nadie.
//!
//! Pinado contra `drivers/irqchip/irq-bcm2712-mip.c` y el nodo del árbol en
//! `bcm2712.dtsi` (`brcm,bcm2712-mip-intc`). **Nada de esto ha corrido**: no
//! hay Pi 5 aquí y QEMU no emula un BCM2712.
//!
//! # Lo que hace el reparto, y lo que no
//!
//! Un dispositivo escribe su mensaje a una dirección **fija del controlador**
//! ([`MipParams::msg_addr`]: `0xff_fffff000` para el `mip0`), y **el vector va
//! en el dato**. No hay una dirección por vector: todos los vectores de un
//! `mip` comparten dirección y se distinguen por lo que escriben. Esto es lo
//! contrario de lo que suele hacerse en x86, donde el dato lleva el vector del
//! LAPIC y la dirección lleva la CPU.
//!
//! El vector se traduce a una **SPI del GIC** sumándole
//! [`MipParams::msi_base`] ([`Mip::spi`]), y **siempre de flanco de subida**:
//! el driver de Linux fija `IRQ_TYPE_EDGE_RISING` en el padre y no ofrece otra
//! cosa. Es la diferencia con las líneas del RP1, que son de nivel y hay que
//! acusar.
//!
//! # La trampa del `msi_offset`
//!
//! Los dos `mip` del BCM2712 no se reparten igual:
//!
//! | | `mip0` | `mip1` |
//! |---|---|---|
//! | SPI base | 128 | 247 |
//! | vectores | 64 | 8 |
//! | desplazamiento | 0 | **8** |
//! | dirección | `0xff_fffff000` | `0xff_ffffe000` |
//!
//! El desplazamiento se suma **después** de reservar el hueco en el mapa, y se
//! resta al liberarlo. O sea que en el `mip1` el mapa tiene **ocho** bits pero
//! los vectores que salen son **8..=15**: un `hwirq` perfectamente legal es
//! *mayor* que el número de vectores. Una comprobación escrita como
//! `hwirq < num_msis` rechazaría todos los vectores del `mip1`, y es la forma
//! natural de escribirla. Ver [`Mip::alloc`] y
//! [`Mip::hwirq_valido`].
//!
//! (El nodo del `mip1` declara ocho de los veinte que dice tener, con el
//! comentario de que los otros son «sparse and non-consecutive». Aquí se
//! declaran los ocho que declara Linux, no los veinte.)
//!
//! # Las máscaras, y por qué están al revés del RP1
//!
//! Un bit a **uno** en una máscara quiere decir **tapado**, y el arranque deja
//! la máscara del host **a cero** — o sea todo destapado — y la del VPU a unos,
//! para que los mensajes no se los lleve el VPU. En el RP1 hay que **encender**
//! cada línea; aquí hay que no taparla, y el estado inicial ya es el bueno.
//!
//! # Los registros que Linux define y no usa
//!
//! Dos familias:
//!
//! * `RAISED` y `CLEARED`, y los cuatro `STATUS`, **no se leen ni se escriben
//!   nunca**. Igual que `INTSTATL`/`INTSTATH` del RP1: están ahí para quien
//!   escriba un bucle de «mira el estado y reparte», que no es como esto
//!   funciona.
//! * la máscara **por vector** se escribe una vez en el arranque y nunca más:
//!   las dos funciones que la tocaban (`mip_irq_domain_activate` y su
//!   `deactivate`) están **dentro de un `#if 0`** y desconectadas del
//!   `irq_domain_ops`. Tapar y destapar un vector lo hacen el GIC padre y
//!   `pci_msi_mask_irq`, no este registro.
//!
//! Y hay una razón para no fiarse de ese código desactivado: **las dos
//! funciones no coinciden entre sí**. `activate` indexa el bit con
//! `d->hwirq % 32` y `deactivate` con `(d->hwirq - msi_base) % 32`, que para
//! un `msi_base` de 128 son bits distintos. Una de las dos está mal, y como
//! ninguna corre, no hay forma de saber cuál. Aquí se expone el cálculo
//! ([`Mip::mask_reg`], [`Mip::mask_bit`]) **sin usarlo**, documentado, para que
//! quien lo necesite no herede la duda sin verla.

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicU64, Ordering};

/// Interrupciones levantadas. **Linux no lo lee nunca.**
pub const INT_RAISED: usize = 0x00;
/// Interrupciones ya atendidas. **Linux no lo lee nunca.**
pub const INT_CLEARED: usize = 0x10;
/// Configuración del host, vectores 0..32.
pub const INT_CFGL_HOST: usize = 0x20;
/// Y 32..64.
pub const INT_CFGH_HOST: usize = 0x30;
/// Máscara del host, vectores 0..32. Un bit a **uno** tapa.
pub const INT_MASKL_HOST: usize = 0x40;
/// Y 32..64.
pub const INT_MASKH_HOST: usize = 0x50;
/// Máscara del VPU, vectores 0..32.
pub const INT_MASKL_VPU: usize = 0x60;
/// Y 32..64.
pub const INT_MASKH_VPU: usize = 0x70;
/// Estado del host, 0..32. **Linux no lo lee nunca.**
pub const INT_STATUSL_HOST: usize = 0x80;
/// Y 32..64. **Tampoco.**
pub const INT_STATUSH_HOST: usize = 0x90;
/// Estado del VPU, 0..32. **Tampoco.**
pub const INT_STATUSL_VPU: usize = 0xa0;
/// Y 32..64. **Tampoco.**
pub const INT_STATUSH_VPU: usize = 0xb0;

/// Lo que mide la ventana de registros, según el `reg` del árbol
/// (`0x10 0x00130000 0x0 0xc0`).
pub const WIN_LEN: usize = 0xc0;

/// Cuántos vectores caben en un par de registros `L`/`H`.
pub const MAX_VECTORES: u32 = 64;

// La ventana tiene que llegar al último registro, y ni un byte más: doce
// registros de 32 bits con paso 0x10 son exactamente 0xc0.
const _: () = assert!(INT_STATUSH_VPU + 4 <= WIN_LEN);
const _: () = assert!(INT_STATUSH_VPU + 0x10 == WIN_LEN);
// Y el paso entre un registro y el siguiente es 0x10, no 4: los registros no
// están pegados, que es el error fácil al teclear el mapa.
const _: () = assert!(INT_CLEARED - INT_RAISED == 0x10);
const _: () = assert!(INT_MASKH_HOST - INT_MASKL_HOST == 0x10);

/// Lo que distingue un `mip` de otro, y que sale del árbol de dispositivos.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MipParams {
    /// La SPI del GIC en la que empiezan sus vectores (`brcm,msi-base-spi`).
    pub msi_base: u32,
    /// Cuántos vectores reparte (`brcm,msi-num-spis`).
    pub num_msis: u32,
    /// Lo que se suma al hueco reservado para dar el vector
    /// (`brcm,msi-offset`).
    pub msi_offset: u32,
    /// La dirección a la que un dispositivo escribe su mensaje
    /// (`brcm,msi-pci-addr`). **La misma para todos sus vectores.**
    pub msg_addr: u64,
}

/// El `mip0`, el que sirve al puente del que cuelga el RP1.
pub const MIP0: MipParams = MipParams {
    msi_base: 128,
    num_msis: 64,
    msi_offset: 0,
    msg_addr: 0xff_fffff000,
};

/// El `mip1`, el del otro puente. Es el que tiene desplazamiento.
pub const MIP1: MipParams = MipParams {
    msi_base: 247,
    num_msis: 8,
    msi_offset: 8,
    msg_addr: 0xff_ffffe000,
};

// Los dos caben en los 64 bits del mapa, contando el desplazamiento: si no,
// `alloc` devolvería un vector que no se puede apuntar.
const _: () = assert!(MIP0.num_msis + MIP0.msi_offset <= MAX_VECTORES);
const _: () = assert!(MIP1.num_msis + MIP1.msi_offset <= MAX_VECTORES);

/// Un reparto de MSI del BCM2712.
pub struct Mip {
    base: usize,
    params: MipParams,
    /// Qué huecos del mapa están cogidos. **Sin el desplazamiento aplicado**:
    /// el bit `n` es el hueco `n`, que da el vector `n + msi_offset`.
    mapa: AtomicU64,
}

impl Mip {
    /// # Safety
    ///
    /// `base` tiene que ser la ventana de registros del `mip` ya mapeada, de
    /// al menos [`WIN_LEN`] bytes.
    pub const unsafe fn new(base: usize, params: MipParams) -> Self {
        Self {
            base,
            params,
            mapa: AtomicU64::new(0),
        }
    }

    /// Los parámetros con los que se construyó.
    pub fn params(&self) -> MipParams {
        self.params
    }

    fn escribe(&self, off: usize, val: u32) {
        #[cfg(test)]
        mip_fake::apunta(off, val);
        unsafe { write_volatile((self.base + off) as *mut u32, val) }
    }

    /// Lee un registro. No lo usa el arranque: está para poder mirar.
    pub fn lee(&self, off: usize) -> u32 {
        #[cfg(test)]
        mip_fake::apunta_lectura(off);
        unsafe { read_volatile((self.base + off) as *const u32) }
    }

    /// El arranque, en el orden de `mip_of_msi_init`.
    ///
    /// Seis escrituras y ninguna lectura: el estado inicial no se consulta,
    /// se impone. Deja **el host destapado** (máscara a cero), **el VPU
    /// tapado** (a unos) y la configuración a unos.
    ///
    /// Las dos mitades `H` se escriben aunque el `mip1` sólo tenga ocho
    /// vectores: Linux las escribe igual, y un registro de vectores que no
    /// existen no hace daño mientras lo que se escriba los deje tapados para
    /// el VPU.
    pub fn init(&self) {
        self.escribe(INT_MASKL_HOST, 0);
        self.escribe(INT_MASKH_HOST, 0);
        self.escribe(INT_MASKL_VPU, !0);
        self.escribe(INT_MASKH_VPU, !0);
        self.escribe(INT_CFGL_HOST, !0);
        self.escribe(INT_CFGH_HOST, !0);
    }

    /// Reserva un vector y lo devuelve **ya desplazado**.
    ///
    /// `None` cuando no quedan huecos. El valor que sale **puede ser mayor o
    /// igual que `num_msis`** (ver la cabecera del módulo): en el `mip1`, con
    /// ocho vectores y desplazamiento ocho, salen 8..=15.
    pub fn alloc(&self) -> Option<u32> {
        let n = self.params.num_msis;
        loop {
            let viejo = self.mapa.load(Ordering::Relaxed);
            let hueco = (0..n).find(|h| viejo & (1u64 << h) == 0)?;
            let nuevo = viejo | (1u64 << hueco);
            if self
                .mapa
                .compare_exchange_weak(viejo, nuevo, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return Some(hueco + self.params.msi_offset);
            }
        }
    }

    /// Libera un vector, que entra **desplazado** como salió de [`alloc`].
    ///
    /// [`alloc`]: Mip::alloc
    ///
    /// Devuelve `false` si ese vector no es de este `mip`, en vez de restar el
    /// desplazamiento a un número más pequeño que él y liberar un hueco ajeno.
    pub fn free(&self, hwirq: u32) -> bool {
        if !self.hwirq_valido(hwirq) {
            return false;
        }
        let hueco = hwirq - self.params.msi_offset;
        self.mapa.fetch_and(!(1u64 << hueco), Ordering::Relaxed);
        true
    }

    /// Si `hwirq` es un vector de este `mip`.
    ///
    /// **No es `hwirq < num_msis`.** El rango es
    /// `msi_offset ..= msi_offset + num_msis - 1`, que para el `mip1` es
    /// 8..=15 con `num_msis` igual a ocho.
    pub fn hwirq_valido(&self, hwirq: u32) -> bool {
        hwirq >= self.params.msi_offset && hwirq < self.params.msi_offset + self.params.num_msis
    }

    /// La SPI del GIC que levanta este vector.
    pub fn spi(&self, hwirq: u32) -> Option<u32> {
        if !self.hwirq_valido(hwirq) {
            return None;
        }
        Some(hwirq + self.params.msi_base)
    }

    /// El mensaje que hay que programar en el dispositivo para este vector:
    /// dirección y dato.
    ///
    /// La dirección es la del controlador, igual para todos sus vectores; el
    /// vector va en el **dato**.
    pub fn msg(&self, hwirq: u32) -> Option<(u64, u32)> {
        if !self.hwirq_valido(hwirq) {
            return None;
        }
        Some((self.params.msg_addr, hwirq))
    }

    /// En qué registro de máscara del host cae este vector.
    ///
    /// **Esto no lo usa nadie**, y a propósito: ver la cabecera del módulo.
    /// Las dos funciones de Linux que lo calculaban están desactivadas y **no
    /// coinciden entre sí**, así que aquí se toma la de `activate` (el vector
    /// tal cual, sin restar `msi_base`), que es la que tiene sentido con el
    /// `hwirq` que esta estructura reparte.
    pub fn mask_reg(&self, hwirq: u32) -> Option<usize> {
        if !self.hwirq_valido(hwirq) {
            return None;
        }
        Some(if hwirq < 32 {
            INT_MASKL_HOST
        } else {
            INT_MASKH_HOST
        })
    }

    /// Y qué bit dentro de ese registro. Un **uno tapa**.
    pub fn mask_bit(&self, hwirq: u32) -> Option<u32> {
        if !self.hwirq_valido(hwirq) {
            return None;
        }
        Some(1u32 << (hwirq % 32))
    }
}

/// Cómo se dispara un vector del `mip`: **siempre** de flanco de subida.
///
/// No es una opción: `mip_irq_domain_alloc` fija `IRQ_TYPE_EDGE_RISING` en el
/// padre para cada vector que reserva, y no hay camino que ponga otra cosa.
/// Está como función y no como comentario porque es justo lo que alguien que
/// venga del RP1 — donde las líneas son de nivel y hay que acusarlas — va a
/// dar por configurable.
pub const fn es_de_flanco(_hwirq: u32) -> bool {
    true
}

#[cfg(test)]
#[path = "bcm2712_mip/mip_tests.rs"]
mod mip_tests;

/// La traza de accesos. Lo que no recuerda un banco de registros es **qué no
/// se tocó**: el arranque son seis escrituras y ninguna lectura, y los seis
/// registros que Linux define y no usa tienen que quedarse sin visitar.
#[cfg(test)]
pub(super) mod mip_fake {
    use core::cell::RefCell;

    /// `(offset, Some(valor))` una escritura; `(offset, None)` una lectura.
    pub type Entrada = (usize, Option<u32>);

    std::thread_local! {
        pub static TRAZA: RefCell<alloc::vec::Vec<Entrada>> =
            const { RefCell::new(alloc::vec::Vec::new()) };
    }

    pub fn apunta(off: usize, val: u32) {
        TRAZA.with(|t| t.borrow_mut().push((off, Some(val))));
    }

    pub fn apunta_lectura(off: usize) {
        TRAZA.with(|t| t.borrow_mut().push((off, None)));
    }

    /// Corre `f` con la traza limpia y devuelve lo que se haya apuntado.
    pub fn trazando<T>(f: impl FnOnce() -> T) -> (T, alloc::vec::Vec<Entrada>) {
        TRAZA.with(|t| t.borrow_mut().clear());
        let r = f();
        (r, TRAZA.with(|t| core::mem::take(&mut *t.borrow_mut())))
    }
}
