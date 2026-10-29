//! Lo que hace falta para que la capa de MSI-X de [`super`] llegue a un
//! dispositivo de verdad: encontrar la capacidad, encontrar la BAR donde vive
//! la tabla, y armar el primer vector.
//!
//! [`super`] sabe de registros y de orden; aquí está lo que hay que averiguar
//! del dispositivo antes de poder tocarlos. Son tres cosas, y cada una tiene
//! una trampa propia.
//!
//! # El `BIR` es un índice de **registro**, no de BAR
//!
//! `BIR` vale 0..=5 y nombra el registro en `0x10 + 4*BIR`. Eso **no** es «la
//! BAR número N»: una BAR de 64 bits se come dos registros, así que en un
//! dispositivo cuya BAR0 es de 64 bits, el registro 1 es la mitad alta de la
//! BAR0 y ningún `BIR` legal lo nombra. Por eso aquí no se cuentan BAR: se
//! multiplica el `BIR` por cuatro y se suma, que es lo que dice la norma, y es
//! además lo que hace que `read_bar_addr` devuelva lo correcto solo
//! (lee el flag de 64 bits del propio registro).
//!
//! # La tabla puede no caber en la BAR que dice
//!
//! Un `QSIZE` o un offset mal leídos dan una tabla que se sale, y programarla
//! sería escribir fuera de la BAR, encima de registros del dispositivo. Se
//! comprueba con [`super::MsixCap::table_fits`] contra el **tamaño sondeado**
//! de esa BAR, no contra un tamaño supuesto, y si no cabe no se arma nada.
//!
//! # MSI ya está encendido cuando llegamos
//!
//! `enable_with` arma MSI durante el `enable` de cualquier
//! dispositivo, así que cuando esto corre la función ya tiene MSI puesto. Y
//! los dos no pueden estar encendidos a la vez (PCI 3.0 §6.8.2), así que lo
//! primero que se hace, una vez se sabe que MSI-X **se va a poder** armar, es
//! apagar MSI con [`super::disable_msi`]. En ese orden: apagar MSI y luego
//! fallar dejaría la función sin ninguna de las dos.
//!
//! # La escotilla
//!
//! Nada de esto ha armado un vector en una máquina. La capa de abajo está
//! probada contra una tabla en memoria del host y un espacio de configuración
//! falso, que contestan igual que el silicio para lo que estos registros son,
//! pero **que un dispositivo real entregue el mensaje** es otra cosa y no se
//! sabe hasta arrancarlo. Así que va detrás de `pci.msix`, apagada por
//! defecto: con la escotilla cerrada no se lee ni la capacidad, y la función
//! se queda con el MSI que ya tenía.

use super::{disable_msi, MsixCap, MsixTable};
use pci::{CSpaceAccessMethod, Location, PortOps};

#[cfg(not(test))]
use core::sync::atomic::{AtomicBool, Ordering};

#[cfg(not(test))]
static ENABLED: AtomicBool = AtomicBool::new(false);

// **Por hilo** en los tests, global en el kernel: una escotilla global haría
// que un test que la abre decidiera el resultado de otro que corre a la vez,
// y los tests de este crate corren en paralelo.
#[cfg(test)]
std::thread_local! {
    static ENABLED: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}

/// Abre la escotilla (`pci.msix`). Por defecto está cerrada.
pub fn set_enabled(v: bool) {
    #[cfg(not(test))]
    ENABLED.store(v, Ordering::Relaxed);
    #[cfg(test)]
    ENABLED.with(|e| e.set(v));
}

/// Si la escotilla está abierta.
pub fn enabled() -> bool {
    #[cfg(not(test))]
    return ENABLED.load(Ordering::Relaxed);
    #[cfg(test)]
    ENABLED.with(|e| e.get())
}

/// El id de la capacidad MSI, para poder apagarla.
pub const CAP_ID_MSI: u8 = 0x05;

/// Hasta dónde se recorre la cadena de capacidades antes de darla por rota.
pub const MAX_CAP_STEPS: usize = 64;

/// La primera capacidad legal del espacio de configuración: por debajo de
/// `0x40` está la cabecera, así que un puntero ahí es una cadena rota.
pub const MIN_CAP_PTR: u16 = 0x40;

/// Dónde está el registro de la BAR que nombra `bir`.
///
/// `bir` es un índice de **registro**, no de BAR: ver la cabecera del módulo.
pub const fn bar_reg_de_bir(bir: u32) -> u16 {
    0x10 + 4 * bir as u16
}

/// Las dos capacidades que importan aquí, si están.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    /// Dónde está MSI, para poder apagarlo.
    pub msi: Option<u16>,
    /// Dónde está MSI-X.
    pub msix: Option<u16>,
}

/// Recorre la cadena de capacidades y apunta dónde están MSI y MSI-X.
///
/// Los tres cortes son los mismos que ya usa `enable_with`: un
/// número de pasos máximo, un puntero que no avanza, y un puntero por debajo
/// de [`MIN_CAP_PTR`]. Una cadena rota se recorre sola para siempre, y esto
/// corre en el camino de arranque.
///
/// # Safety
///
/// `ops` tiene que direccionar el espacio de configuración de `loc`.
pub unsafe fn find_caps<T: PortOps>(ops: &T, am: CSpaceAccessMethod, loc: Location) -> Caps {
    let mut caps = Caps::default();
    let mut cap_ptr = unsafe { am.read8(ops, loc, 0x34) } as u16;
    let mut previo = 0u16;
    let mut pasos = 0usize;
    while cap_ptr > 0 {
        if pasos >= MAX_CAP_STEPS || cap_ptr == previo || cap_ptr < MIN_CAP_PTR {
            break;
        }
        pasos += 1;
        match unsafe { am.read8(ops, loc, cap_ptr) } {
            CAP_ID_MSI if caps.msi.is_none() => caps.msi = Some(cap_ptr),
            super::CAP_ID_MSIX if caps.msix.is_none() => caps.msix = Some(cap_ptr),
            _ => {}
        }
        previo = cap_ptr;
        cap_ptr = unsafe { am.read8(ops, loc, cap_ptr + 1) } as u16;
    }
    caps
}

/// La dirección del mensaje de un LAPIC de x86: la ventana `0xfee0_0000` con
/// el id del destino en los bits 19:12.
///
/// Es la misma dirección que el camino de MSI ya escribe; está aquí para que
/// las dos la saquen del mismo sitio.
pub const fn lapic_msg_addr(dest: u32) -> u64 {
    0xfee0_0000u64 | ((dest as u64) << 12)
}

/// Lo que hace falta saber de una BAR y que sólo la máquina contesta.
pub trait BarInfo {
    /// La dirección física de la BAR del registro `bar_reg`, o 0 si no hay.
    fn addr(&mut self, bar_reg: u16) -> u64;
    /// Su tamaño en bytes, o 0 si no se puede sondear.
    fn len(&mut self, bar_reg: u16) -> u64;
    /// Mapea `len` bytes desde `pa` y devuelve la virtual.
    fn map(&mut self, pa: u64, len: usize) -> usize;
}

/// Por qué no se armó MSI-X. Son todos casos de «este dispositivo no», no de
/// error: con cualquiera de ellos la función se queda con el MSI que tenía.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoHubo {
    /// La escotilla está cerrada.
    Escotilla,
    /// El dispositivo no declara la capacidad.
    SinCapacidad,
    /// El `BIR` de la tabla no es una BAR (6 y 7 están reservados).
    BirReservado,
    /// La BAR que dice no está programada.
    BarSinDireccion,
    /// No se pudo sondear su tamaño.
    BarSinTamano,
    /// La tabla no cabe en esa BAR.
    TablaNoCabe,
    /// El dispositivo declara la capacidad pero sin vectores alcanzables.
    SinVector,
}

/// Lo que sale de un armado que salió bien.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Armado {
    /// La capacidad tal como la declaró el dispositivo.
    pub cap: MsixCap,
    /// Las cuatro palabras de la entrada 0 **releídas del dispositivo**.
    ///
    /// Es lo único que dice, sin esperar una interrupción, que la tabla estaba
    /// donde la capacidad decía: con el `BIR` o el offset mal, lo escrito cae
    /// en otro sitio de la BAR y esto no devuelve lo que se puso.
    pub entrada: [u32; 4],
}

/// Arma el vector 0 de MSI-X en `loc`, si se puede.
///
/// El orden es el de la cabecera: averiguarlo todo primero, apagar MSI
/// después, armar al final. Devuelve la capacidad leída, para que quien llame
/// pueda decir en el log cuántos vectores tenía.
///
/// # Safety
///
/// `ops` tiene que direccionar el espacio de configuración de `loc`, y `bar`
/// tiene que devolver mapeos válidos.
pub unsafe fn arm_first_vector<T: PortOps, B: BarInfo>(
    ops: &T,
    am: CSpaceAccessMethod,
    loc: Location,
    dest: u32,
    irq: u32,
    bar: &mut B,
) -> Result<Armado, NoHubo> {
    if !enabled() {
        return Err(NoHubo::Escotilla);
    }
    let caps = unsafe { find_caps(ops, am, loc) };
    let cap_ptr = caps.msix.ok_or(NoHubo::SinCapacidad)?;
    let cap = unsafe { MsixCap::read(ops, am, loc, cap_ptr) }.ok_or(NoHubo::BirReservado)?;

    let bar_reg = bar_reg_de_bir(cap.table_bir);
    let pa = bar.addr(bar_reg);
    if pa == 0 {
        return Err(NoHubo::BarSinDireccion);
    }
    let len = bar.len(bar_reg);
    if len == 0 {
        return Err(NoHubo::BarSinTamano);
    }
    if !cap.table_fits(len) {
        return Err(NoHubo::TablaNoCabe);
    }

    // Sólo ahora, que ya se sabe que se va a poder, se apaga MSI: al revés,
    // un fallo de los de arriba dejaría la función sin ninguna de las dos.
    if let Some(msi) = caps.msi {
        unsafe { disable_msi(ops, am, loc, msi) };
    }

    let va = bar.map(
        pa,
        (cap.table_offset as usize) + cap.table_size * super::ENTRY_SIZE,
    );
    let tabla = unsafe { MsixTable::new(va + cap.table_offset as usize, cap.table_size) };
    if unsafe {
        super::arm(
            ops,
            am,
            loc,
            &cap,
            &tabla,
            0,
            lapic_msg_addr(dest),
            irq + 32,
        )
    } {
        let entrada = tabla.read_back(0).ok_or(NoHubo::SinVector)?;
        Ok(Armado { cap, entrada })
    } else {
        Err(NoHubo::SinVector)
    }
}

#[cfg(test)]
#[path = "bringup/bringup_tests.rs"]
mod bringup_tests;
