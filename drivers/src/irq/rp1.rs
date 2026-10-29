//! El bloque de configuración de interrupciones del RP1, el chip de
//! entrada/salida de la Raspberry Pi 5.
//!
//! Es la tercera pieza de lo que hace falta para que el Ethernet de la Pi 5
//! conteste, detrás del puente PCIe del BCM2712
//! ([`crate::bus::brcmstb`]): el GEM vive dentro del RP1 y su línea de
//! interrupción es la 6 del RP1, no una SPI del GIC.
//!
//! # Lo primero que hay que desaprender: aquí no hay un controlador
//!
//! El nodo del árbol **es el bus mismo**:
//!
//! ```text
//! rp1: rp1 {
//!     compatible = "simple-bus";
//!     #interrupt-cells = <2>;
//!     interrupt-controller;
//!     interrupt-parent = <&rp1>;
//! };
//! ```
//!
//! Sin `reg`, sin `compatible` propio de controlador y sin registro de estado
//! que decir «ha saltado la línea N». Porque **no hay despacho**: las 61
//! líneas del RP1 son **61 vectores MSI-X**, uno por línea, y el que llega ya
//! dice cuál es. Linux pide `pci_alloc_irq_vectors(pdev, 61, 61,
//! PCI_IRQ_MSIX)` — mínimo y máximo iguales, así que menos de 61 es un fallo
//! duro — y lo único que hace su `interrupt-controller` es la traducción de
//! dos celdas: la línea N del RP1 es el vector N.
//!
//! Eso significa que el `INTSTATL`/`INTSTATH` (0x108/0x10c) que el driver de
//! Linux define **no lo usa nadie**: son defines muertos. Quien venga a
//! escribir el típico bucle «lee el estado y reparte» los encontrará y hará lo
//! que no se debe. Ver [`INTSTATL`].
//!
//! # Lo que sí hay que programar
//!
//! Un registro de configuración por vector, con **tres alias de la misma
//! ventana**: lectura-escritura en `0x000`, «pon estos bits» en `0x800` y
//! «quita estos bits» en `0xc00`. Nunca se lee para modificar: se escribe el
//! bit en el alias que toca. Es el mismo patrón que los resets del BCM2712
//! ([`crate::bus::brcmstb::reset`]), y por la misma razón: un uno manda y un
//! cero no dice nada.
//!
//! Y de los cuatro bits de ese registro, el que importa de verdad es el
//! acuse. **Una línea de nivel hay que acusarla después del manejador**
//! ([`Rp1IrqCfg::ack`]); una de flanco, no. El GEM del RP1 es de nivel
//! (`IRQ_TYPE_LEVEL_HIGH` en su nodo), así que esto es justo lo que decide si
//! el Ethernet interrumpe una vez o para siempre.
//!
//! # Lo que esta tanda NO hace
//!
//! - **Pedir los 61 vectores MSI-X.** Eclipse sabe de MSI, no de MSI-X, y el
//!   lado del puente lo sirve `brcm,bcm2712-mip-intc`, del que aquí no hay
//!   nada. Sin eso, ninguna de estas líneas llega a la CPU.
//! - **La máscara.** Cada línea tiene **dos puertas independientes**: el bit
//!   [`MSIX_CFG_ENABLE`] de aquí y la máscara MSI del lado PCIe, que Linux
//!   toca con `pci_msi_mask_irq`. Esta tanda sólo tiene la primera.
//! - Registrar las líneas en el reparto de interrupciones del kernel.
//!
//! # De dónde salen los números
//!
//! `drivers/mfd/rp1.c`, `include/dt-bindings/mfd/rp1.h` y
//! `arch/arm64/boot/dts/broadcom/rp1.dtsi` de la rama `rpi-6.6.y`. **Nada de
//! esto ha corrido contra el silicio**: no hay Pi 5, y QEMU no emula el RP1.

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicU64, Ordering};

// --- dónde vive el bloque, dentro de BAR1 ----------------------------------

/// `RP1_SYSINFO_BASE`: offset del bloque de identificación dentro de BAR1.
pub const SYSINFO_BASE: usize = 0x00_0000;
/// `RP1_PCIE_APBS_BASE`: offset del bloque de configuración de MSI-X dentro
/// de BAR1. Es el que mapea este módulo.
pub const PCIE_APBS_BASE: usize = 0x10_8000;

/// `SYSINFO_CHIP_ID_OFFSET`, dentro de [`SYSINFO_BASE`].
pub const SYSINFO_CHIP_ID: usize = 0x0;
/// `SYSINFO_PLATFORM_OFFSET`.
pub const SYSINFO_PLATFORM: usize = 0x4;
/// `RP1_B0_CHIP_ID`.
pub const CHIP_ID_B0: u32 = 0x1000_1927;
/// `RP1_C0_CHIP_ID`, el único que el driver de Linux acepta.
pub const CHIP_ID_C0: u32 = 0x2000_1927;
/// `RP1_PLATFORM_FPGA`: con este bit puesto, lo de enfrente es la FPGA y no
/// el chip.
pub const PLATFORM_FPGA: u32 = 1 << 0;

/// BAR1 por debajo de esto es el RP1 **sin firmware cargado**, y el driver de
/// Linux se niega a seguir («Not initialised - is the firmware running?»). No
/// es un tamaño mínimo arbitrario: es la forma de ver que el firmware de la
/// Pi no ha arrancado el chip.
pub const BAR1_MIN_LEN: u64 = 0x1_0000;

// --- los tres alias de la ventana ------------------------------------------

/// `REG_RW`: lectura y escritura directa.
pub const REG_RW: usize = 0x000;
/// `REG_SET`: lo que se escriba aquí **pone** esos bits.
pub const REG_SET: usize = 0x800;
/// `REG_CLR`: lo que se escriba aquí **quita** esos bits.
pub const REG_CLR: usize = 0xc00;

/// `MSIX_CFG(x)`: el registro de configuración del vector `x`. **Empieza en 8,
/// no en 0**, así que el del vector 0 está en `0x8` y su alias de poner en
/// `0x808`, no en `0x800`.
pub const fn msix_cfg(vector: usize) -> usize {
    0x8 + 4 * vector
}

/// `MSIX_CFG_IACK_EN`: esta línea es de nivel y hay que acusarla.
pub const MSIX_CFG_IACK_EN: u32 = 1 << 3;
/// `MSIX_CFG_IACK`: el acuse. Se escribe en [`REG_SET`] **después** del
/// manejador, y sólo en una línea de nivel.
pub const MSIX_CFG_IACK: u32 = 1 << 2;
/// `MSIX_CFG_TEST`: dispara la línea a mano. Aquí no se usa.
pub const MSIX_CFG_TEST: u32 = 1 << 1;
/// `MSIX_CFG_ENABLE`: la puerta de **este** lado. La otra es la máscara MSI
/// del lado PCIe.
pub const MSIX_CFG_ENABLE: u32 = 1 << 0;

/// `INTSTATL`, y [`INTSTATH`] su mitad alta: los dos registros de estado que
/// el driver de Linux **define y no lee nunca**.
///
/// Están aquí para que quede dicho, no para usarlos: con un vector MSI-X por
/// línea no hay nada que repartir, el vector que llega ya dice qué línea es.
/// Un bucle que leyese el estado y repartiese sería correr dos veces la misma
/// línea, o ninguna.
pub const INTSTATL: usize = 0x108;
/// La mitad alta de [`INTSTATL`]. Igual de inútil.
pub const INTSTATH: usize = 0x10c;

// Los alias no se pisan: el registro del último vector cae muy por debajo de
// los 0x400 que separan `REG_SET` de `REG_CLR`.
const _: () = assert!(msix_cfg(INT_END - 1) < REG_SET);
const _: () = assert!(REG_CLR - REG_SET == 0x400);

// --- las líneas ------------------------------------------------------------

/// `RP1_INT_END`: cuántas líneas hay. Linux pide **exactamente** estos
/// vectores MSI-X, ni uno menos.
pub const INT_END: usize = 61;

/// Con cuántos bits se queda el manejador encadenado de Linux
/// (`desc->irq_data.hwirq & 0x3f`): **seis**, o sea 64 vectores, aunque sólo
/// existan 61.
pub const VECTOR_MASK: u32 = 0x3f;

/// Las 61 líneas, en el orden de `dt-bindings/mfd/rp1.h`.
///
/// Se guardan como tabla y no sólo como constantes sueltas para poder
/// comprobar que son una permutación densa de `0..61`: la lista de Linux está
/// escrita a mano y **tiene el 57 fuera de sitio** (`RP1_INT_PROC_MISC` va
/// detrás del 60), que es justo la forma que tiene un número de salir mal
/// copiado.
pub const LINEAS: [(&str, usize); INT_END] = [
    ("io_bank0", 0),
    ("io_bank1", 1),
    ("io_bank2", 2),
    ("audio_in", 3),
    ("audio_out", 4),
    ("pwm0", 5),
    ("eth", 6),
    ("i2c0", 7),
    ("i2c1", 8),
    ("i2c2", 9),
    ("i2c3", 10),
    ("i2c4", 11),
    ("i2c5", 12),
    ("i2c6", 13),
    ("i2s0", 14),
    ("i2s1", 15),
    ("i2s2", 16),
    ("sdio0", 17),
    ("sdio1", 18),
    ("spi0", 19),
    ("spi1", 20),
    ("spi2", 21),
    ("spi3", 22),
    ("spi4", 23),
    ("spi5", 24),
    ("uart0", 25),
    ("timer_0", 26),
    ("timer_1", 27),
    ("timer_2", 28),
    ("timer_3", 29),
    ("usbhost0", 30),
    ("usbhost0_0", 31),
    ("usbhost0_1", 32),
    ("usbhost0_2", 33),
    ("usbhost0_3", 34),
    ("usbhost1", 35),
    ("usbhost1_0", 36),
    ("usbhost1_1", 37),
    ("usbhost1_2", 38),
    ("usbhost1_3", 39),
    ("dma", 40),
    ("pwm1", 41),
    ("uart1", 42),
    ("uart2", 43),
    ("uart3", 44),
    ("uart4", 45),
    ("uart5", 46),
    ("mipi0", 47),
    ("mipi1", 48),
    ("video_out", 49),
    ("pio_0", 50),
    ("pio_1", 51),
    ("adc_fifo", 52),
    ("pcie_out", 53),
    ("spi6", 54),
    ("spi7", 55),
    ("spi8", 56),
    ("proc_misc", 57),
    ("syscfg", 58),
    ("clocks_default", 59),
    ("vbusctrl", 60),
];

/// `RP1_INT_ETH`: la del GEM, que es la que esto existe para poder levantar.
pub const INT_ETH: usize = 6;
/// `RP1_INT_SYSCFG`.
pub const INT_SYSCFG: usize = 58;
/// `RP1_INT_PCIE_OUT`.
pub const INT_PCIE_OUT: usize = 53;

/// Que la tabla sea una permutación densa de `0..INT_END`.
const fn tabla_densa() -> bool {
    let mut vistos = 0u64;
    let mut i = 0;
    while i < INT_END {
        let n = LINEAS[i].1;
        if n >= INT_END {
            return false;
        }
        let bit = 1u64 << n;
        if vistos & bit != 0 {
            return false;
        }
        vistos |= bit;
        i += 1;
    }
    vistos == (1u64 << INT_END) - 1
}
const _: () = assert!(
    tabla_densa(),
    "la tabla de lineas tiene un hueco o un repetido"
);

// --- cómo se dispara una línea ---------------------------------------------

/// Los dos únicos disparos que el RP1 acepta.
///
/// Linux contesta `-EINVAL` a cualquier otro `IRQ_TYPE_*`; aquí es un tipo con
/// dos variantes, así que «cualquier otro» no se puede ni escribir.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disparo {
    /// `IRQ_TYPE_LEVEL_HIGH`. **Hay que acusarla** después del manejador.
    Nivel,
    /// `IRQ_TYPE_EDGE_RISING`. No se acusa.
    Flanco,
}

/// El bloque de configuración de MSI-X del RP1.
pub struct Rp1IrqCfg {
    /// BAR1 ya mapeada, más [`PCIE_APBS_BASE`].
    base: usize,
    /// Qué líneas se han declarado de nivel, que son las que hay que acusar.
    /// Es estado porque el acuse ocurre en el manejador, lejos de donde se
    /// declaró el disparo, y el registro no lo dice: `IACK_EN` es lo que el
    /// chip hace, no lo que el driver tiene que recordar hacer.
    de_nivel: AtomicU64,
}

impl Rp1IrqCfg {
    /// # Safety
    ///
    /// `base` tiene que ser BAR1 del RP1 ya mapeada más [`PCIE_APBS_BASE`], y
    /// BAR1 tiene que medir más de [`BAR1_MIN_LEN`] (si no, el firmware de la
    /// Pi no ha arrancado el chip y el bloque no contesta).
    pub const unsafe fn new(base: usize) -> Self {
        Self {
            base,
            de_nivel: AtomicU64::new(0),
        }
    }

    fn escribe(&self, off: usize, val: u32) {
        #[cfg(test)]
        rp1_fake::apunta(off, val);
        unsafe { write_volatile((self.base + off) as *mut u32, val) }
    }

    /// El único sitio donde se lee, y no es para modificar: el registro
    /// directo de un vector, para poder mirarlo.
    pub fn lee_cfg(&self, vector: usize) -> u32 {
        let off = REG_RW + msix_cfg(vector);
        #[cfg(test)]
        rp1_fake::apunta_lectura(off);
        unsafe { read_volatile((self.base + off) as *const u32) }
    }

    fn pon(&self, vector: usize, bits: u32) {
        self.escribe(REG_SET + msix_cfg(vector), bits);
    }

    fn quita(&self, vector: usize, bits: u32) {
        self.escribe(REG_CLR + msix_cfg(vector), bits);
    }

    /// Declara cómo se dispara una línea.
    ///
    /// Devuelve `false` si la línea no existe. Para una de nivel pone
    /// [`MSIX_CFG_IACK_EN`] **y lo apunta**, porque el acuse de después del
    /// manejador depende de ello; para una de flanco lo quita y lo desapunta.
    pub fn set_disparo(&self, linea: usize, disparo: Disparo) -> bool {
        if linea >= INT_END {
            return false;
        }
        match disparo {
            Disparo::Nivel => {
                self.pon(linea, MSIX_CFG_IACK_EN);
                self.de_nivel.fetch_or(1 << linea, Ordering::Relaxed);
            }
            Disparo::Flanco => {
                self.quita(linea, MSIX_CFG_IACK_EN);
                self.de_nivel.fetch_and(!(1u64 << linea), Ordering::Relaxed);
            }
        }
        true
    }

    /// Si esa línea se declaró de nivel.
    pub fn es_de_nivel(&self, linea: usize) -> bool {
        linea < INT_END && self.de_nivel.load(Ordering::Relaxed) & (1 << linea) != 0
    }

    /// Abre la puerta de **este** lado. La otra es la máscara MSI del lado
    /// PCIe, que no está aquí.
    pub fn habilita(&self, linea: usize) -> bool {
        if linea >= INT_END {
            return false;
        }
        self.pon(linea, MSIX_CFG_ENABLE);
        true
    }

    /// La cierra.
    pub fn deshabilita(&self, linea: usize) -> bool {
        if linea >= INT_END {
            return false;
        }
        self.quita(linea, MSIX_CFG_ENABLE);
        true
    }

    /// El acuse de después del manejador. **No hace nada en una línea de
    /// flanco**, y no es que sobre: acusar una de flanco sería escribir un bit
    /// que el chip no espera.
    ///
    /// Devuelve si se acusó.
    pub fn ack(&self, linea: usize) -> bool {
        if !self.es_de_nivel(linea) {
            return false;
        }
        self.pon(linea, MSIX_CFG_IACK);
        true
    }
}

/// La traducción de dos celdas del nodo del RP1: la línea N **es** el vector
/// MSI-X N. No hay tabla; el `interrupt-controller` del árbol es sólo esto.
pub const fn linea_a_vector(linea: usize) -> usize {
    linea
}

/// Y la vuelta, tal como la hace el manejador encadenado de Linux: con los
/// seis bits de abajo. Un vector de 61 o más no es una línea.
pub const fn vector_a_linea(vector: u32) -> Option<usize> {
    let n = (vector & VECTOR_MASK) as usize;
    if n < INT_END {
        Some(n)
    } else {
        None
    }
}

#[cfg(test)]
#[path = "rp1/rp1_tests.rs"]
mod rp1_tests;

/// La traza de accesos, que es lo único que un banco de registros no
/// recuerda: con tres alias de la misma ventana, lo que hay que comprobar es
/// **en qué alias** se escribió, y que no se leyó para modificar.
#[cfg(test)]
pub(super) mod rp1_fake {
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
