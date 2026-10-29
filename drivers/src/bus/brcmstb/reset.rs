//! Los dos controladores de reset de los que cuelga el puente PCIe del
//! BCM2712.
//!
//! El puente no tiene su propio registro de reset. En la variante genérica de
//! la familia STB sí lo tiene (`PCIE_RGR1_SW_INIT_1`), y por eso la tabla de
//! offsets del 2712 **no trae esa entrada**: aquí el reset vive fuera, en dos
//! bloques distintos que el árbol declara aparte.
//!
//! ```text
//! pcie2: pcie@120000 {
//!     resets      = <&bcm_reset 32>, <&bcm_reset 44>, <&pcie_rescal>;
//!     reset-names = "swinit", "bridge", "rescal";
//! };
//!
//! pcie_rescal: reset-controller@119500 {
//!     compatible = "brcm,bcm7216-pcie-sata-rescal";
//!     reg = <0x10 0x00119500 0x0 0x10>;
//!     #reset-cells = <0>;
//! };
//!
//! bcm_reset: reset-controller@1504318 {
//!     compatible = "brcm,brcmstb-reset";
//!     reg = <0x10 0x01504318 0x0 0x30>;
//!     #reset-cells = <1>;
//! };
//! ```
//!
//! # De dónde salen los números
//!
//! De `drivers/reset/reset-brcmstb.c` y `drivers/reset/reset-brcmstb-rescal.c`
//! del árbol de Raspberry Pi (rama `rpi-6.6.y`). Son dos drivers de ciento y
//! pocas líneas cada uno, y el nombre de cada constante de Linux va escrito al
//! lado del offset.
//!
//! # Qué NO está validado
//!
//! **Nada de esto ha corrido en una Pi 5.** Lo único comprobable aquí es qué
//! registro se toca y con qué bits; eso es lo que miden los tests, contra un
//! banco de registros falso.

use core::ptr::{read_volatile, write_volatile};

const MODULE: &str = "brcm-reset";

/// Espera activa de `us` microsegundos, con la misma salida que el resto de los
/// drivers del árbol: un tope de vueltas, para que un reloj que no avanza deje
/// el arranque con un retardo corto de más en vez de colgar el núcleo.
fn udelay(us: u64) {
    #[cfg(test)]
    reset_fake::ESPERADO.with(|c| c.set(c.get() + us));
    if us == 0 {
        return;
    }
    const MAX_SPINS: u64 = 1_000_000;
    let ahora = || unsafe { crate::bus::drivers_timer_now_as_micros() };
    let t0 = ahora();
    let mut n = 0u64;
    while ahora().wrapping_sub(t0) < us {
        core::hint::spin_loop();
        n += 1;
        if n >= MAX_SPINS {
            break;
        }
    }
}

// --- `brcm,brcmstb-reset`, el de estilo SW_INIT -----------------------------

/// `SW_INIT_SET`: escribir un uno **aprieta** el reset de esa línea.
pub const SW_INIT_SET: usize = 0x00;
/// `SW_INIT_CLEAR`: escribir un uno lo **suelta**.
pub const SW_INIT_CLEAR: usize = 0x04;
/// `SW_INIT_STATUS`.
pub const SW_INIT_STATUS: usize = 0x08;

/// `SW_INIT_BANK_SIZE`.
///
/// **0x18, no 0x0c.** Un banco son tres registros de los que este driver usa
/// los tres, pero el banco siguiente no empieza detrás del tercero: el bloque
/// trae registros de más que nadie toca y que *cuentan como parte del banco*.
/// Con un paso de 0x0c el banco 1 caería encima del `STATUS` del banco 0.
pub const SW_INIT_BANK_SIZE: usize = 0x18;

/// `SW_INIT_BANK(id)`: la línea `id` vive en el banco `id / 32`.
pub const fn sw_init_bank(id: u32) -> usize {
    (id >> 5) as usize
}

/// `SW_INIT_BIT(id)`: y dentro de su banco es el bit `id % 32`.
pub const fn sw_init_bit(id: u32) -> u32 {
    1 << (id & 0x1f)
}

/// Las dos líneas que el puente PCIe pide a `bcm_reset`, con el nombre que les
/// da `reset-names`.
pub const RESET_SWINIT: u32 = 32;
/// La segunda: el reset del puente propiamente dicho.
pub const RESET_BRIDGE: u32 = 44;

/// El controlador de resets de estilo SW_INIT.
#[derive(Debug)]
pub struct BrcmstbReset {
    base: usize,
    /// Cuántos bancos caben en la ventana que el nodo declara. En el BCM2712
    /// son `0x30 / 0x18 = 2`.
    banks: usize,
}

impl BrcmstbReset {
    /// Toma la ventana ya mapeada. `len` es el tamaño del `reg` del nodo, del
    /// que sale cuántos bancos hay: una línea de un banco que no está en la
    /// ventana se rechaza en vez de escribir fuera.
    ///
    /// # Safety
    /// `base` tiene que apuntar a la ventana del controlador, mapeada como
    /// dispositivo y de al menos `len` bytes.
    pub const unsafe fn new(base: usize, len: usize) -> Self {
        Self {
            base,
            banks: len / SW_INIT_BANK_SIZE,
        }
    }

    /// El offset del registro `reg` del banco de la línea `id`, o `None` si esa
    /// línea cae fuera de la ventana.
    fn off(&self, id: u32, reg: usize) -> Option<usize> {
        let bank = sw_init_bank(id);
        if bank >= self.banks {
            warn!(
                "{}: la linea {} esta en el banco {}, y la ventana solo tiene {}",
                MODULE, id, bank, self.banks
            );
            return None;
        }
        Some(bank * SW_INIT_BANK_SIZE + reg)
    }

    /// `brcmstb_reset_assert`: aprieta el reset de la línea `id`.
    ///
    /// Se escribe **sólo** el bit de esa línea, y en el registro `SET`. Estos
    /// dos registros son de orden, no de estado: un uno manda, un cero no dice
    /// nada, así que no hay que leer antes ni conservar los demás bits — y
    /// escribir aquí la máscara entera apretaría el reset de treinta y una
    /// líneas más.
    pub fn assert_line(&self, id: u32) -> bool {
        match self.off(id, SW_INIT_SET) {
            Some(off) => {
                self.write(off, sw_init_bit(id));
                true
            }
            None => false,
        }
    }

    /// `brcmstb_reset_deassert`: suelta el reset de la línea `id`.
    ///
    /// Va a **otro registro** (`CLEAR`), no al mismo con un cero. Y después se
    /// espera: el bloque tarda hasta unos 14 µs en volver a funcionar tras
    /// soltarle el reset, y Linux deja 100 µs de margen. Tocar sus registros
    /// antes de eso lee basura sin dar error.
    pub fn deassert_line(&self, id: u32) -> bool {
        match self.off(id, SW_INIT_CLEAR) {
            Some(off) => {
                self.write(off, sw_init_bit(id));
                udelay(DEASSERT_SETTLE_US);
                true
            }
            None => false,
        }
    }

    /// `brcmstb_reset_status`: si la línea `id` está en reset ahora mismo.
    pub fn line_is_asserted(&self, id: u32) -> Option<bool> {
        let off = self.off(id, SW_INIT_STATUS)?;
        Some(self.read(off) & sw_init_bit(id) != 0)
    }

    fn read(&self, off: usize) -> u32 {
        unsafe { read_volatile((self.base + off) as *const u32) }
    }

    fn write(&self, off: usize, val: u32) {
        unsafe { write_volatile((self.base + off) as *mut u32, val) }
    }
}

/// Lo que Linux espera tras soltar un reset: el peor caso son ~14 µs, y deja
/// 100 µs de margen.
pub const DEASSERT_SETTLE_US: u64 = 100;

// --- `brcm,bcm7216-pcie-sata-rescal`, el recalibrado del PHY ----------------

/// `BRCM_RESCAL_START`.
pub const RESCAL_START: usize = 0x0;
/// `BRCM_RESCAL_START_BIT`.
pub const RESCAL_START_BIT: u32 = 1 << 0;
/// `BRCM_RESCAL_CTRL`.
pub const RESCAL_CTRL: usize = 0x4;
/// `BRCM_RESCAL_STATUS`.
pub const RESCAL_STATUS: usize = 0x8;
/// `BRCM_RESCAL_STATUS_BIT`.
pub const RESCAL_STATUS_BIT: u32 = 1 << 0;

/// Cuántas veces se mira el estado del recalibrado, y cada cuánto.
///
/// Linux lo escribe como `readl_poll_timeout(..., 100, 1000)`: 100 µs entre
/// vueltas y 1000 µs de tope, o sea **diez vueltas**.
pub const RESCAL_POLL_TRIES: u32 = 10;
/// El paso entre vueltas.
pub const RESCAL_POLL_STEP_US: u64 = 100;

/// Por qué no salió el recalibrado.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RescalError {
    /// El bit de arranque no se quedó puesto. No es un error de escritura: es
    /// el bloque diciendo que no arranca.
    NoArranca,
    /// Arrancó y no acabó dentro del plazo.
    SeAgotoElPlazo,
}

/// El recalibrado del PHY de PCIe/SATA, que el árbol declara como un
/// controlador de reset con `#reset-cells = <0>`.
#[derive(Debug)]
pub struct PcieRescal {
    base: usize,
}

impl PcieRescal {
    /// # Safety
    /// `base` tiene que apuntar a la ventana del bloque (`0x10` bytes), mapeada
    /// como dispositivo.
    pub const unsafe fn new(base: usize) -> Self {
        Self { base }
    }

    /// `brcm_rescal_reset_set`, que en Linux es a la vez el `.reset` y el
    /// `.deassert` del controlador.
    ///
    /// Tres cosas que no se parecen a un reset normal:
    ///
    /// 1. El bit de arranque **se relee**. Si no se quedó puesto, el bloque no
    ///    va a arrancar y seguir adelante es esperar un recalibrado que nunca
    ///    pasa. Esto no es una comprobación de que la escritura llegó: es un
    ///    acuse del bloque.
    /// 2. Se espera a que el estado diga que acabó, no un plazo fijo.
    /// 3. El bit de arranque **se vuelve a limpiar** al final. Es un pulso, no
    ///    un pestillo: dejarlo puesto deja el bloque pidiendo otro
    ///    recalibrado.
    ///
    /// Y el `assert` de este controlador es un **no-op a propósito** — ver
    /// [`PcieRescal::assert_noop`].
    pub fn start(&self) -> Result<(), RescalError> {
        let reg = self.read(RESCAL_START);
        self.write(RESCAL_START, reg | RESCAL_START_BIT);

        if self.read(RESCAL_START) & RESCAL_START_BIT == 0 {
            warn!("{}: el recalibrado de PCIe/SATA no arranca", MODULE);
            return Err(RescalError::NoArranca);
        }

        let mut listo = false;
        for _ in 0..RESCAL_POLL_TRIES {
            if self.read(RESCAL_STATUS) & RESCAL_STATUS_BIT != 0 {
                listo = true;
                break;
            }
            udelay(RESCAL_POLL_STEP_US);
        }

        // El pulso se retira pase lo que pase: si se agotó el plazo, dejarlo
        // puesto es peor que soltarlo.
        let reg = self.read(RESCAL_START);
        self.write(RESCAL_START, reg & !RESCAL_START_BIT);

        if !listo {
            warn!("{}: se agoto el plazo del recalibrado de PCIe/SATA", MODULE);
            return Err(RescalError::SeAgotoElPlazo);
        }
        Ok(())
    }

    /// El `.assert` de este controlador **no hace nada**, y así está en Linux:
    /// todo el trabajo lo hace [`PcieRescal::start`]. Está aquí para que quede
    /// escrito que la ausencia es deliberada y no un hueco por rellenar.
    pub fn assert_noop(&self) {}

    fn read(&self, off: usize) -> u32 {
        unsafe { read_volatile((self.base + off) as *const u32) }
    }

    fn write(&self, off: usize, val: u32) {
        #[cfg(test)]
        let val = reset_fake::filtra(off, val);
        unsafe { write_volatile((self.base + off) as *mut u32, val) }
    }
}

/// La única parte del silicio que un banco de registros de mentira no puede
/// imitar: un bit que **no se queda** donde se escribe.
#[cfg(test)]
mod reset_fake {
    use core::cell::Cell;

    std::thread_local! {
        /// Con esto puesto, el bit de arranque del recalibrado no se queda
        /// puesto --- que es el bloque diciendo que no arranca, no un fallo de
        /// la escritura. Va por hilo para que el banco de pruebas pueda correr
        /// en paralelo.
        pub static RECHAZA_ARRANQUE: Cell<bool> = const { Cell::new(false) };

        /// Cuantos microsegundos se ha esperado en este hilo. Una espera no
        /// deja rastro en un banco de registros, y la de tras soltar un reset
        /// es parte de lo que el bloque necesita, no un adorno: sin esto no
        /// hay forma de comprobar que esta.
        pub static ESPERADO: Cell<u64> = const { Cell::new(0) };
    }

    /// Pone el contador a cero y devuelve lo que se haya esperado al acabar.
    pub fn esperando<T>(f: impl FnOnce() -> T) -> (T, u64) {
        ESPERADO.with(|c| c.set(0));
        let r = f();
        (r, ESPERADO.with(|c| c.get()))
    }

    pub fn filtra(off: usize, val: u32) -> u32 {
        if off == super::RESCAL_START && RECHAZA_ARRANQUE.with(|c| c.get()) {
            val & !super::RESCAL_START_BIT
        } else {
            val
        }
    }
}

#[cfg(test)]
mod reset_tests;
