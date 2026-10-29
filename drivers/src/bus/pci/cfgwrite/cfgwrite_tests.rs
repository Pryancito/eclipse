//! Lo que hay que fijar aquí es una cuenta de dos bits, y la razón de fijarla
//! es que el crate la hace mal sin que se note: el valor acaba en el campo
//! vecino de la misma palabra, el campo que se quería cambiar se queda como
//! estaba, y nadie devuelve un error.

use super::*;
use lock::Mutex;

struct FakeCfg {
    space: Mutex<[u32; 64]>,
}

impl FakeCfg {
    fn new() -> Self {
        Self {
            space: Mutex::new([0u32; 64]),
        }
    }
    fn pon(&self, off: u16, val: u32) {
        self.space.lock()[off as usize / 4] = val;
    }
    fn saca(&self, off: u16) -> u32 {
        self.space.lock()[off as usize / 4]
    }
}

impl PortOps for FakeCfg {
    unsafe fn read8(&self, port: u16) -> u8 {
        (self.saca(port & !0b11) >> ((port as usize & 0b11) * 8)) as u8
    }
    unsafe fn read16(&self, port: u16) -> u16 {
        (self.saca(port & !0b11) >> ((port as usize & 0b10) * 8)) as u16
    }
    unsafe fn read32(&self, port: u32) -> u32 {
        self.saca((port & 0xff) as u16)
    }
    unsafe fn write8(&self, _port: u16, _val: u8) {
        unreachable!("sólo se usa la palabra entera")
    }
    unsafe fn write16(&self, _port: u16, _val: u16) {
        unreachable!("sólo se usa la palabra entera")
    }
    unsafe fn write32(&self, port: u32, val: u32) {
        self.pon((port & 0xff) as u16, val)
    }
}

const AM: CSpaceAccessMethod = CSpaceAccessMethod::MemoryMapped(core::ptr::null_mut());
const LOC: Location = Location {
    bus: 0,
    device: 0,
    function: 0,
};

/// Lo que hace el crate: `offset as usize & 0b10 << 3`, que por precedencia es
/// `offset & (0b10 << 3)`.
const fn shift16_del_crate(offset: u16) -> u32 {
    (offset as u32) & (0b10 << 3)
}

/// Y `offset as usize & 0b11 << 3`.
const fn shift8_del_crate(offset: u16) -> u32 {
    (offset as u32) & (0b11 << 3)
}

// --- la cuenta -------------------------------------------------------------

/// El desplazamiento lo deciden los bits **1 y 0** del offset. El del crate lo
/// deciden los bits **4 y 3**, que es otra cosa.
#[test]
fn the_shift_comes_from_the_two_low_bits_and_not_from_bits_four_and_three() {
    assert_eq!(shift_de_16(0x04), 0, "mitad baja");
    assert_eq!(shift_de_16(0x06), 16, "mitad alta");
    assert_eq!(shift_de_8(0x4c), 0);
    assert_eq!(shift_de_8(0x4d), 8);
    assert_eq!(shift_de_8(0x4e), 16);
    assert_eq!(shift_de_8(0x4f), 24);
}

/// **El caso que importa**: con `cap_ptr = 0x60`, el registro de control de
/// una capacidad está en `0x62`, que es la mitad **alta** de la palabra. El
/// crate lo pondría en la baja — donde viven el id de la capacidad y el
/// puntero al siguiente.
#[test]
fn a_capability_control_at_cap_ptr_sixty_is_where_the_crate_gets_it_wrong() {
    assert_eq!(shift_de_16(0x62), 16, "la mitad alta");
    assert_eq!(shift16_del_crate(0x62), 0, "el crate, la baja");
    assert_ne!(shift_de_16(0x62), shift16_del_crate(0x62));
}

/// Y el caso que **coincide por casualidad**, que es por lo que el bug no ha
/// saltado nunca: `cap_ptr = 0x70`, y los dos offsets que este kernel usa a
/// todas horas.
#[test]
fn the_offsets_this_kernel_uses_all_the_time_happen_to_agree() {
    for off in [0x04u16, 0x3e, 0x72] {
        assert_eq!(
            shift_de_16(off),
            shift16_del_crate(off),
            "{:#x} coincide, y por eso nunca ha saltado",
            off
        );
    }
}

/// Los bytes que el driver de audio escribe **no** coinciden: dos de los tres.
#[test]
fn two_of_the_three_bytes_the_audio_driver_writes_do_not_agree() {
    assert_ne!(shift_de_8(0x4c), shift8_del_crate(0x4c), "0x4c");
    assert_ne!(shift_de_8(0x4e), shift8_del_crate(0x4e), "0x4e");
    assert_eq!(
        shift_de_8(0x4d),
        shift8_del_crate(0x4d),
        "0x4d, de casualidad"
    );
}

// --- qué se escribe de verdad ----------------------------------------------

/// Escribir el control de una capacidad en `0x62` cambia la mitad alta y
/// **deja el id y el puntero al siguiente intactos**.
#[test]
fn writing_a_capability_control_leaves_the_id_and_next_pointer_alone() {
    let cfg = FakeCfg::new();
    // id 0x11 (MSI-X), siguiente 0x80, control a cero.
    cfg.pon(0x60, 0x0000_8011);
    unsafe { cfg_write16(&cfg, AM, LOC, 0x62, 0xc000) };
    let dw = cfg.saca(0x60);
    assert_eq!(dw & 0xff, 0x11, "el id");
    assert_eq!((dw >> 8) & 0xff, 0x80, "el puntero al siguiente");
    assert_eq!(dw >> 16, 0xc000, "el control");
}

/// Y el registro de comando en `0x04` cambia la mitad baja sin tocar el
/// estado, que es la alta.
#[test]
fn writing_the_command_register_does_not_touch_the_status_beside_it() {
    let cfg = FakeCfg::new();
    cfg.pon(0x04, 0x0290_0000);
    unsafe { cfg_write16(&cfg, AM, LOC, 0x04, 0x0007) };
    assert_eq!(cfg.saca(0x04), 0x0290_0007);
}

/// Un byte va a su byte, sin llevarse los otros tres.
#[test]
fn a_byte_goes_to_its_own_byte() {
    for (off, esperado) in [
        (0x4cu16, 0x4433_22aau32),
        (0x4d, 0x4433_aa11),
        (0x4e, 0x44aa_2211),
        (0x4f, 0xaa33_2211),
    ] {
        let cfg = FakeCfg::new();
        cfg.pon(0x4c, 0x4433_2211);
        unsafe { cfg_write8(&cfg, AM, LOC, off, 0xaa) };
        assert_eq!(cfg.saca(0x4c), esperado, "byte {:#x}", off);
    }
}

/// Una escritura de 16 bits a un offset **impar** se redondea a la media
/// palabra alineada, que es lo mismo que hace el camino de lectura. No es
/// legal en PCI; lo que no se hace es inventar un comportamiento nuevo.
#[test]
fn a_misaligned_half_word_write_rounds_down_like_the_read_path_does() {
    assert_eq!(shift_de_16(0x05), shift_de_16(0x04));
    assert_eq!(shift_de_16(0x07), shift_de_16(0x06));
    let cfg = FakeCfg::new();
    cfg.pon(0x04, 0xffff_ffff);
    unsafe { cfg_write16(&cfg, AM, LOC, 0x05, 0x1234) };
    assert_eq!(cfg.saca(0x04), 0xffff_1234);
}

/// La palabra a la que se va es la que contiene el offset, no la de al lado.
/// (El crate también acierta en esto: lo que falla es sólo el desplazamiento
/// dentro de ella.)
#[test]
fn the_word_written_is_the_one_that_contains_the_offset() {
    assert_eq!(palabra_de(0x62), 0x60);
    assert_eq!(palabra_de(0x4f), 0x4c);
    assert_eq!(palabra_de(0x04), 0x04);
    let cfg = FakeCfg::new();
    cfg.pon(0x64, 0xdead_beef);
    unsafe { cfg_write16(&cfg, AM, LOC, 0x62, 0x1111) };
    assert_eq!(cfg.saca(0x64), 0xdead_beef, "se tocó la palabra de al lado");
}

/// Y el trozo que no es se queda como estaba, bit a bit.
#[test]
fn only_the_field_asked_for_changes() {
    assert_eq!(con_campo(0xffff_ffff, 0x0000_ffff, 0), 0xffff_0000);
    assert_eq!(
        con_campo(0x0000_0000, 0xffff_0000, 0xaaaa_0000),
        0xaaaa_0000
    );
    // Un valor con bits fuera de la máscara no se cuela.
    assert_eq!(con_campo(0, 0x0000_ff00, 0xffff_ffff), 0x0000_ff00);
}
