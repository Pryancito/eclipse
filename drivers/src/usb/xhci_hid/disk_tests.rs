use super::*;
use crate::scheme::BlockScheme;

/// Un disco de mentira: sin controlador detras (`Weak::new()` no sube
/// nunca), que es justo lo que hace falta para probar la aritmetica de
/// traduccion sin tocar hardware.
fn fake_disk(dev_blocks: u64, dev_block_size: u32) -> UsbDisk {
    UsbDisk {
        ctrl: Weak::new(),
        disk_id: 1,
        name: alloc::string::String::from("usbdisk-test"),
        dev_blocks,
        dev_block_size,
    }
}

#[test]
fn el_cdb_de_read10_lleva_el_lba_y_la_cuenta_en_big_endian() {
    // SBC-3 §5.10: LBA en los bytes 2..=5 y cuenta en 7..=8, los dos en
    // big-endian. Con little-endian el disco lee de otro sitio y nadie
    // avisa, asi que el orden se comprueba byte a byte.
    let cdb = scsi_rw10(SCSI_READ_10, 0x0123_4567, 0x0abc).expect("cabe en el CDB");
    assert_eq!(cdb[0], 0x28);
    assert_eq!(&cdb[2..6], &[0x01, 0x23, 0x45, 0x67]);
    assert_eq!(&cdb[7..9], &[0x0a, 0xbc]);
    assert_eq!(cdb[9], 0, "byte de control");
}

#[test]
fn write10_solo_cambia_el_opcode() {
    let r = scsi_rw10(SCSI_READ_10, 42, 8).unwrap();
    let w = scsi_rw10(SCSI_WRITE_10, 42, 8).unwrap();
    assert_eq!(w[0], 0x2a);
    assert_eq!(r[1..], w[1..]);
}

#[test]
fn una_peticion_que_no_cabe_en_el_cdb_se_rechaza_no_se_recorta() {
    // Las tres que no se pueden expresar. Recortarlas escribiria en el
    // sitio equivocado o dejaria media escritura hecha.
    assert!(scsi_rw10(SCSI_READ_10, 0, 65_536).is_none(), "cuenta > u16");
    assert!(
        scsi_rw10(SCSI_READ_10, u32::MAX as u64 + 1, 1).is_none(),
        "LBA > u32"
    );
    assert!(scsi_rw10(SCSI_READ_10, 0, 0).is_none(), "cero bloques");
    // Y el borde de los dos, que SI cabe.
    assert!(scsi_rw10(SCSI_READ_10, u32::MAX as u64, 65_535).is_some());
}

#[test]
fn sync_cache_de_cero_bloques_es_hasta_el_final() {
    let cdb = scsi_sync_cache10();
    assert_eq!(cdb[0], 0x35);
    // LBA 0 y cuenta 0: toda la unidad (SBC-3 §5.26).
    assert_eq!(&cdb[2..6], &[0, 0, 0, 0]);
    assert_eq!(&cdb[7..9], &[0, 0]);
}

#[test]
fn con_bloques_de_512_un_sector_del_kernel_es_un_bloque_de_la_unidad() {
    let d = fake_disk(2048, 512);
    assert_eq!(d.dev_span(0, 512), Some((0, 1, 0)));
    assert_eq!(d.dev_span(512 * 7, 512), Some((7, 1, 0)));
    assert_eq!(d.dev_span(512 * 7, 4096), Some((7, 8, 0)));
    assert_eq!(d.block_count(), 2048);
    assert_eq!(d.logical_block_size(), 512);
}

#[test]
fn con_bloques_de_4096_un_sector_del_kernel_es_un_cuarto_de_bloque() {
    let d = fake_disk(1000, 4096);
    // El sector 7 del kernel cae DENTRO del bloque 0 de la unidad, a 3584
    // bytes del principio. Pedirle «el bloque 7» leeria 4 KiB de otro
    // sitio.
    assert_eq!(d.dev_span(512 * 7, 512), Some((0, 1, 3584)));
    assert_eq!(d.dev_span(4096, 512), Some((1, 1, 0)));
    // Una peticion a caballo de dos bloques son dos bloques.
    assert_eq!(d.dev_span(512 * 7, 1024), Some((0, 2, 3584)));
    // La capacidad se anuncia en sectores de 512, no en bloques.
    assert_eq!(d.block_count(), 8000);
    assert_eq!(d.logical_block_size(), 4096);
}

#[test]
fn un_buffer_que_no_es_multiplo_de_512_se_rechaza() {
    // El convenio de `BlockScheme` es sectores de 512 enteros. Un bufer
    // corto que pasara llegaria al disco como un CBW que promete un sector
    // con medio sector detras.
    let d = fake_disk(2048, 512);
    let mut corto = [0u8; 500];
    assert_eq!(
        d.read_block(0, &mut corto).unwrap_err(),
        DeviceError::InvalidParam
    );
    assert_eq!(
        d.write_block(0, &corto).unwrap_err(),
        DeviceError::InvalidParam
    );
    let mut vacio: [u8; 0] = [];
    assert_eq!(
        d.read_block(0, &mut vacio).unwrap_err(),
        DeviceError::InvalidParam
    );
}

#[test]
fn sin_controlador_detras_la_peticion_valida_dice_que_no_esta_listo() {
    // Desenchufar no es un fallo de E/S: `NotReady` y no `IoError`, para
    // que quien monta sepa que el disco se fue en vez de darlo por roto.
    let d = fake_disk(2048, 512);
    let mut buf = [0u8; 512];
    assert_eq!(
        d.read_block(0, &mut buf).unwrap_err(),
        DeviceError::NotReady
    );
    assert_eq!(d.write_block(0, &buf).unwrap_err(), DeviceError::NotReady);
    assert_eq!(d.flush().unwrap_err(), DeviceError::NotReady);
}

#[test]
fn la_fase_de_datos_sabe_su_longitud_y_su_sentido() {
    let mut dst = [0u8; 1024];
    assert_eq!(MscData::Read(&mut dst).len(), 1024);
    assert!(!MscData::Read(&mut dst).is_write());
    let src = [0u8; 512];
    assert_eq!(MscData::Write(&src).len(), 512);
    assert!(MscData::Write(&src).is_write());
}

#[test]
fn una_vuelta_cabe_en_un_trb_y_en_bloques_enteros() {
    // La cuenta de `msc_rw`, con el bufer entero disponible.
    for (bs, esperado) in [(512usize, 127usize), (4096, 15), (2048, 31)] {
        let per_turn = ((MSC_MAX_TRB_LEN as usize).min(MSC_BOUNCE_BYTES) / bs).max(1);
        assert_eq!(per_turn, esperado, "bloques de {bs}");
        assert!(
            per_turn * bs <= MSC_MAX_TRB_LEN as usize,
            "una vuelta con bloques de {} no cabe en un TRB Normal",
            bs
        );
        assert!(per_turn <= u16::MAX as usize);
    }
}

#[test]
fn el_bufer_de_rebote_cubre_una_vuelta_entera() {
    // Si el bufer fuese menor que el campo del TRB, `per_turn` saldria del
    // bufer y la ultima vuelta copiaria menos de lo que promete el CBW.
    assert!(MSC_BOUNCE_BYTES >= MSC_MAX_TRB_LEN as usize);
}
