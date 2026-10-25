//! El motor del AHCI: programar el puerto, someter un comando, leer el
//! IDENTIFY y mover un bloque.
//!
//! Hasta aquí este fichero se probaba por aritmética: el FIS que se codifica,
//! el PRDT que se arma, los límites que se rechazan. Lo que no se probaba es
//! lo que pasa cuando hay un controlador delante, y ahí viven las decisiones
//! que no se pueden tomar en otro sitio: de qué capacidad es el disco según
//! lo que contesta el IDENTIFY, qué ocurre cuando un comando no vuelve, y
//! —lo que de verdad importa— si los bytes acaban en el sector que el
//! llamante pidió. Una lectura que trae el sector de al lado vuelve con
//! `Ok(())`.
//!
//! Nada de esto corre en CI: eclipse arranca en QEMU desde una imagen en RAM,
//! así que el camino del AHCI no ejecuta una sola línea allí. En una máquina
//! de verdad es el sistema de ficheros raíz.

use super::ahci_fake::{Disk, Faults, Hw};
use super::*;
use crate::nvme::nvme_queue::test_clock;

/// El reloj tiene que avanzar: cada espera de este fichero lo lee hasta su
/// plazo, y con el paso a cero una espera sin cumplir no acaba nunca.
fn reloj() {
    test_clock::set_auto_advance(1_000);
}

/// Un HBA con un puerto y un disco de `sectors` sectores de 512 bytes.
fn con_disco(sectors: usize) -> (Hw, Vec<AhciInterface>) {
    reloj();
    let hw = Hw::new(1, Some(Disk::of_512(sectors)), true);
    let disks = AhciInterface::new_all(hw.base(), 0).expect("el HBA enumera su disco");
    (hw, disks)
}

// --- la enumeración -------------------------------------------------------

/// Un puerto con un disco acaba en un `AhciInterface` con su capacidad en las
/// unidades de 512 bytes que habla `BlockScheme`, que es lo que el resto del
/// kernel va a creer que mide el disco.
#[test]
fn un_puerto_con_disco_sale_enumerado_con_su_capacidad() {
    let (_hw, disks) = con_disco(2048);

    assert_eq!(disks.len(), 1, "no ha salido exactamente un disco");
    assert_eq!(disks[0].name(), "ahci-0");
    assert_eq!(disks[0].block_count(), 2048);
    assert_eq!(disks[0].logical_block_size(), 512);
}

/// Los puertos que el HBA no implementa no se miran: `PI` es la lista, y
/// recorrer los 32 a ciegas programaría registros que no existen.
#[test]
fn solo_se_miran_los_puertos_que_el_hba_implementa() {
    reloj();
    // Implementados el 0 y el 3, con disco en los dos (el fake pone el mismo).
    let hw = Hw::new(0b1001, Some(Disk::of_512(1024)), true);
    let disks = AhciInterface::new_all(hw.base(), 0).expect("enumera");

    let nombres: Vec<&str> = disks.iter().map(|d| d.name()).collect();
    assert_eq!(nombres, alloc::vec!["ahci-0", "ahci-3"], "{:?}", nombres);
}

/// Un puerto deshabilitado a propósito (DET=4) se salta sin tocarlo.
#[test]
fn un_puerto_deshabilitado_se_salta() {
    reloj();
    let hw = Hw::new(1, Some(Disk::of_512(1024)), true);
    hw.poke_port(0, PORT_SSTS, 0x0000_0134);

    assert_eq!(
        AhciInterface::new_all(hw.base(), 0).err(),
        Some(DeviceError::NoResources),
        "ha enumerado un puerto deshabilitado"
    );
    // Y se salta antes de tocarlo: un puerto deshabilitado al que se le
    // programa el motor se come el segundo de espera del enlace para nada,
    // y eso en el arranque se nota.
    assert_eq!(
        hw.peek_port(0, PORT_CLB),
        0,
        "ha programado un puerto deshabilitado"
    );
}

/// Un puerto cuya firma no es de ATA no se enumera: ahí puede haber un
/// lector óptico (ATAPI), que no habla este protocolo.
#[test]
fn una_firma_que_no_es_de_ata_no_se_enumera() {
    reloj();
    let hw = Hw::new(1, Some(Disk::of_512(1024)), true);
    hw.poke_port(0, PORT_SIG, 0xEB14_0101); // ATAPI

    assert_eq!(
        AhciInterface::new_all(hw.base(), 0).err(),
        Some(DeviceError::NoResources),
        "ha enumerado un ATAPI como si fuera un disco"
    );
}

/// Un disco que dice tener cero sectores se descarta: dejarlo pasar daría un
/// dispositivo de bloques de capacidad cero que el resto del kernel aceptaría.
#[test]
fn un_disco_de_cero_sectores_se_descarta() {
    reloj();
    let mut d = Disk::of_512(1024);
    d.lba28_sectors = Some(0);
    d.lba48_sectors = Some(0);
    let hw = Hw::new(1, Some(d), true);

    assert_eq!(
        AhciInterface::new_all(hw.base(), 0).err(),
        Some(DeviceError::NoResources),
        "ha enumerado un disco de capacidad cero"
    );
}

/// Antes del primer comando el puerto queda programado: la lista de comandos
/// y el área de FIS recibidos en sus registros, y el motor en marcha. Sin eso
/// el controlador haría DMA donde le quedara el reset.
#[test]
fn el_puerto_queda_programado_antes_del_primer_comando() {
    let (hw, _disks) = con_disco(1024);

    assert_ne!(
        hw.peek_port(0, PORT_CLB),
        0,
        "la lista de comandos sin poner"
    );
    assert_ne!(hw.peek_port(0, PORT_FB), 0, "el área de FIS sin poner");
    let cmd = hw.peek_port(0, PORT_CMD);
    assert!(cmd & CMD_ST != 0, "el motor de comandos parado: {:#x}", cmd);
    assert!(cmd & CMD_FRE != 0, "la recepción de FIS parada: {:#x}", cmd);
    assert_eq!(
        hw.peek_port(0, PORT_IE),
        0,
        "ha dejado interrupciones puestas"
    );
}

// --- el IDENTIFY ----------------------------------------------------------

/// La enumeración empieza por un IDENTIFY, y es el primer comando que se
/// somete.
#[test]
fn la_enumeracion_empieza_por_un_identify() {
    let (hw, _disks) = con_disco(1024);

    assert_eq!(hw.opcodes().first().copied(), Some(ATA_CMD_IDENTIFY));
}

/// Un disco 4Kn se cree: la capacidad que declara está en **sus** sectores,
/// de 4096 bytes, así que en las unidades de 512 que habla `BlockScheme` mide
/// ocho veces más. Creerle los sectores sin escalar haría desaparecer siete
/// octavos del disco.
#[test]
fn un_disco_de_4kn_declara_ocho_veces_mas_bloques_de_512() {
    reloj();
    let hw = Hw::new(1, Some(Disk::of_4kn(512)), true);
    let disks = AhciInterface::new_all(hw.base(), 0).expect("enumera");

    assert_eq!(disks[0].logical_block_size(), 4096);
    assert_eq!(disks[0].block_count(), 512 * 8);
}

/// Cuando el IDENTIFY trae las dos cuentas, manda la de 48 bits: la de 28 no
/// puede expresar más de 128 GiB y en un disco grande viene recortada.
#[test]
fn con_las_dos_cuentas_manda_la_de_48_bits() {
    reloj();
    let mut d = Disk::of_512(1024);
    d.lba28_sectors = Some(0x0FFF_FFFF);
    d.lba48_sectors = Some(0x1_0000_0000);
    let hw = Hw::new(1, Some(d), true);
    let disks = AhciInterface::new_all(hw.base(), 0).expect("enumera");

    assert_eq!(disks[0].block_count(), 0x1_0000_0000);
}

/// Y un disco que no soporta LBA de 48 bits se queda con la de 28, aunque las
/// palabras de la otra traigan basura.
#[test]
fn sin_lba_de_48_bits_manda_la_cuenta_de_28() {
    reloj();
    let mut d = Disk::of_512(1024);
    d.lba48 = false;
    d.lba28_sectors = Some(0x0010_0000);
    d.lba48_sectors = Some(0);
    let hw = Hw::new(1, Some(d), true);
    let disks = AhciInterface::new_all(hw.base(), 0).expect("enumera");

    assert_eq!(disks[0].block_count(), 0x0010_0000);
}

/// Un IDENTIFY que no vuelve deja el puerto sin enumerar en vez de con una
/// capacidad inventada.
#[test]
fn un_identify_que_no_vuelve_no_enumera_el_puerto() {
    reloj();
    let hw = Hw::new(1, Some(Disk::of_512(1024)), true);
    hw.faults(Faults {
        hang_commands: 1,
        ..Default::default()
    });

    assert_eq!(
        AhciInterface::new_all(hw.base(), 0).err(),
        Some(DeviceError::NoResources),
        "ha enumerado un disco cuyo IDENTIFY no contestó"
    );
}

// --- leer y escribir ------------------------------------------------------

/// Lo que importa de todo este camino: una lectura trae **ese** sector. El
/// disco falso lleva un patrón distinto en cada uno, así que un error de un
/// solo sector en la aritmética del LBA sale aquí.
#[test]
fn una_lectura_trae_el_sector_que_se_pidio() {
    let (hw, disks) = con_disco(64);
    for s in 0..64u8 {
        hw.fill_disk(s as usize * 512, &[s; 512]);
    }

    let mut buf = [0u8; 512];
    disks[0].read_block(7, &mut buf).expect("la lectura va");

    assert_eq!(buf, [7u8; 512], "ha traído otro sector");
}

/// Y una escritura deja los bytes en ese sector y no en el de al lado.
#[test]
fn una_escritura_deja_los_bytes_en_su_sector() {
    let (hw, disks) = con_disco(64);

    disks[0]
        .write_block(9, &[0xab; 512])
        .expect("la escritura va");

    assert_eq!(hw.disk_bytes(9 * 512, 512), alloc::vec![0xab; 512]);
    assert_eq!(
        hw.disk_bytes(8 * 512, 512),
        alloc::vec![0u8; 512],
        "ha pisado el sector anterior"
    );
    assert_eq!(
        hw.disk_bytes(10 * 512, 512),
        alloc::vec![0u8; 512],
        "ha pisado el sector siguiente"
    );
}

/// Una transferencia de varios sectores va en un solo comando y llega entera.
#[test]
fn varios_sectores_van_en_un_comando_y_llegan_enteros() {
    let (hw, disks) = con_disco(64);
    for s in 0..64u8 {
        hw.fill_disk(s as usize * 512, &[s; 512]);
    }
    let antes = hw.commands();

    let mut buf = [0u8; 4 * 512];
    disks[0].read_block(2, &mut buf).expect("la lectura va");

    assert_eq!(hw.commands() - antes, 1, "lo ha partido en varios comandos");
    for (i, s) in (2u8..6).enumerate() {
        assert_eq!(
            &buf[i * 512..(i + 1) * 512],
            &[s; 512][..],
            "el sector {} ha venido mal",
            s
        );
    }
}

/// Un disco con LBA de 48 bits usa el opcode EXT, y uno sin él el de 28: el
/// aparato antiguo no entiende el otro y contestaría con error.
#[test]
fn cada_modo_de_direccionamiento_somete_su_opcode() {
    let (hw, disks) = con_disco(64);
    disks[0].read_block(0, &mut [0u8; 512]).expect("lee");
    assert!(
        hw.opcodes().contains(&ATA_CMD_READ_DMA_EXT),
        "{:?}",
        hw.opcodes()
    );

    reloj();
    let mut d = Disk::of_512(64);
    d.lba48 = false;
    let hw2 = Hw::new(1, Some(d), true);
    let disks2 = AhciInterface::new_all(hw2.base(), 0).expect("enumera");
    disks2[0].read_block(0, &mut [0u8; 512]).expect("lee");

    assert!(
        hw2.opcodes().contains(&ATA_CMD_READ_DMA),
        "{:?}",
        hw2.opcodes()
    );
}

/// Un `flush` somete el comando de vaciado de caché y no mueve datos: sin él
/// un apagado se lleva por delante lo que el disco tenga en su caché.
#[test]
fn un_flush_somete_el_vaciado_de_cache() {
    let (hw, disks) = con_disco(64);

    disks[0].flush().expect("el flush va");

    assert!(
        hw.opcodes().contains(&ATA_CMD_FLUSH_CACHE_EXT),
        "{:?}",
        hw.opcodes()
    );
}

/// Un error de task file en el comando sale como error de E/S, no como una
/// lectura buena con basura dentro.
#[test]
fn un_error_de_task_file_sale_como_error_de_entrada_salida() {
    let (hw, disks) = con_disco(64);
    hw.faults(Faults {
        tfes_commands: 64,
        ..Default::default()
    });

    assert_eq!(
        disks[0].read_block(0, &mut [0u8; 512]).err(),
        Some(DeviceError::IoError),
        "una lectura con TFES ha salido buena"
    );
}

/// Y un ERR en `PxTFD` **sin** latchar nada en `PxIS` también: es el caso que
/// se le escapaba al driver, porque miraba sólo los bits de `PxIS`.
#[test]
fn un_err_de_tfd_sin_latch_en_is_tambien_falla() {
    let (hw, disks) = con_disco(64);
    hw.faults(Faults {
        tfd_err_commands: 64,
        ..Default::default()
    });

    assert_eq!(
        disks[0].read_block(0, &mut [0u8; 512]).err(),
        Some(DeviceError::IoError),
        "un ERR de TFD sin latch ha salido bueno"
    );
}

/// Un comando que no vuelve se reintenta, y el driver no se queda colgado
/// para siempre: acaba devolviendo error.
#[test]
fn un_comando_que_no_vuelve_se_reintenta_y_acaba_en_error() {
    let (hw, disks) = con_disco(64);
    let antes = hw.commands();
    hw.faults(Faults {
        hang_commands: u32::MAX,
        ..Default::default()
    });

    assert_eq!(
        disks[0].read_block(0, &mut [0u8; 512]).err(),
        Some(DeviceError::IoError)
    );
    assert!(
        hw.commands() - antes > 1,
        "no ha reintentado: {} comandos",
        hw.commands() - antes
    );
}

/// Un comando que falla una vez y va la segunda sale bien: eso es lo que
/// compran los reintentos, y sin ellos un error transitorio tiraría el
/// sistema de ficheros raíz.
#[test]
fn un_fallo_transitorio_se_recupera_en_el_reintento() {
    let (hw, disks) = con_disco(64);
    hw.fill_disk(0, &[0x5a; 512]);
    hw.faults(Faults {
        tfes_commands: 1,
        ..Default::default()
    });

    let mut buf = [0u8; 512];
    disks[0].read_block(0, &mut buf).expect("el reintento va");

    assert_eq!(buf, [0x5a; 512]);
}

/// Una petición más allá del final del disco se rechaza antes de someter
/// nada: el disco contestaría con error, pero el error que se quiere es el
/// del llamante.
#[test]
fn una_peticion_pasado_el_final_no_somete_nada() {
    let (hw, disks) = con_disco(64);
    let antes = hw.commands();

    assert_eq!(
        disks[0].read_block(64, &mut [0u8; 512]).err(),
        Some(DeviceError::InvalidParam)
    );
    assert_eq!(
        hw.commands(),
        antes,
        "ha sometido un comando de todas formas"
    );
}

/// Un puerto cuyo task file se queda ocupado para siempre no se enumera: el
/// IDENTIFY no se puede ni someter, porque el aparato no está listo para
/// recibirlo.
#[test]
fn un_task_file_que_no_se_desocupa_no_enumera_el_puerto() {
    reloj();
    let hw = Hw::new(1, Some(Disk::of_512(1024)), true);
    hw.faults(Faults {
        tfd_stuck_busy: true,
        ..Default::default()
    });

    assert_eq!(
        AhciInterface::new_all(hw.base(), 0).err(),
        Some(DeviceError::NoResources),
        "ha enumerado un puerto con el task file ocupado"
    );
    assert_eq!(
        hw.commands(),
        0,
        "ha sometido un comando con el puerto ocupado"
    );
}

/// Un bit de error latchado corta la espera en el sitio en vez de comerse el
/// plazo entero. Son cinco segundos en el IDENTIFY, y en una máquina con
/// varios puertos se suman al arranque uno detrás de otro.
#[test]
fn un_error_latchado_corta_la_espera_en_vez_de_comerse_el_plazo() {
    reloj();
    let hw = Hw::new(1, Some(Disk::of_512(1024)), true);
    hw.faults(Faults {
        tfes_commands: 1,
        ..Default::default()
    });

    let t0 = test_clock::now();
    let _ = AhciInterface::new_all(hw.base(), 0);
    let gastado = test_clock::now() - t0;

    assert!(
        gastado < IDENTIFY_TIMEOUT_US,
        "se ha comido el plazo entero del IDENTIFY: {} us",
        gastado
    );
}

/// Y cuando el aparato informa del error **sin** soltar el hueco, mirar los
/// bits de error durante la espera es lo único que la corta: con el hueco ya
/// libre la espera acaba igual, así que éste es el caso que lo demuestra.
#[test]
fn un_error_latchado_con_el_hueco_ocupado_tambien_corta_la_espera() {
    reloj();
    let hw = Hw::new(1, Some(Disk::of_512(1024)), true);
    hw.faults(Faults {
        tfes_and_hang: 1,
        ..Default::default()
    });

    let t0 = test_clock::now();
    let _ = AhciInterface::new_all(hw.base(), 0);
    let gastado = test_clock::now() - t0;

    assert!(
        gastado < IDENTIFY_TIMEOUT_US,
        "se ha comido el plazo entero con el error ya latchado: {} us",
        gastado
    );
}

/// Un IDENTIFY que acaba en error no se cree, aunque el aparato haya puesto
/// datos buenos en el buffer: un disco puede hacer DMA de su respuesta y
/// luego informar del fallo, y quedarse con esa capacidad es quedarse con una
/// cifra que el aparato acaba de desdecir.
#[test]
fn un_identify_que_acaba_en_error_no_se_cree() {
    reloj();
    let hw = Hw::new(1, Some(Disk::of_512(1024)), true);
    // El fake rellena el buffer y DESPUÉS levanta TFES: los datos están ahí.
    hw.faults(Faults {
        tfes_commands: 1,
        ..Default::default()
    });

    assert_eq!(
        AhciInterface::new_all(hw.base(), 0).err(),
        Some(DeviceError::NoResources),
        "se ha creído un IDENTIFY que acabó en error"
    );
}

/// Un puerto cuyo enlace no llega a establecerse (DET=1: hay algo, pero no
/// comunica) no se enumera. Es el cable medio suelto, y enumerarlo daría un
/// disco que falla en la primera lectura.
#[test]
fn un_puerto_cuyo_enlace_no_se_establece_no_se_enumera() {
    reloj();
    let hw = Hw::new(1, Some(Disk::of_512(1024)), true);
    hw.poke_port(0, PORT_SSTS, 0x0000_0131); // DET=1

    assert_eq!(
        AhciInterface::new_all(hw.base(), 0).err(),
        Some(DeviceError::NoResources),
        "ha enumerado un puerto sin enlace"
    );
    assert_eq!(hw.commands(), 0, "ha sometido un comando sin enlace");
}

// --- el disco 4Kn, sector a sector ----------------------------------------

/// Un HBA con un puerto y un disco 4Kn de `sectors` sectores de 4096 bytes.
fn con_disco_4kn(sectors: usize) -> (Hw, Vec<AhciInterface>) {
    reloj();
    let hw = Hw::new(1, Some(Disk::of_4kn(sectors)), true);
    let disks = AhciInterface::new_all(hw.base(), 0).expect("el HBA enumera su disco 4Kn");
    (hw, disks)
}

/// Deja en cada trozo de 512 bytes del disco un patrón distinto, para que un
/// error de un solo trozo en la aritmética salga.
fn patron_por_trozos(hw: &Hw, trozos: usize) {
    for t in 0..trozos {
        hw.fill_disk(t * 512, &[(t as u8).wrapping_add(1); 512]);
    }
}

/// `BlockScheme` habla en bloques de 512 y el aparato sólo sabe direccionar
/// sus sectores de 4096: ocho bloques de la interfaz caben en uno del disco,
/// y leer el cuarto significa traerse el sector entero y sacar de dentro el
/// trozo que se pidió. La aritmética está en dos sitios —el LBA que se
/// somete y el desplazamiento dentro del rebote— y equivocar cualquiera de
/// los dos devuelve `Ok(())` con los bytes de al lado.
#[test]
fn una_lectura_de_512_de_un_disco_4kn_sale_de_dentro_de_su_sector() {
    let (hw, disks) = con_disco_4kn(4);
    patron_por_trozos(&hw, 32);
    let antes = hw.commands();

    let mut buf = [0u8; 512];
    disks[0].read_block(3, &mut buf).expect("la lectura va");

    assert_eq!(buf, [4u8; 512], "ha traído otro trozo del sector");
    assert_eq!(
        hw.commands() - antes,
        1,
        "un trozo de un sector es un solo comando"
    );
    assert_eq!(
        hw.opcodes().last(),
        Some(&ATA_CMD_READ_DMA_EXT),
        "{:?}",
        hw.opcodes()
    );
}

/// Y una lectura que empieza en un sector del aparato y acaba en el siguiente
/// se parte en dos, cada mitad con su LBA. Es el caso que una partición mal
/// alineada produce en cada acceso.
#[test]
fn una_lectura_a_caballo_de_dos_sectores_4kn_trae_las_dos_mitades() {
    let (hw, disks) = con_disco_4kn(4);
    patron_por_trozos(&hw, 32);
    let antes = hw.commands();

    // Bloques 7 y 8: el 7 es el último trozo del sector 0 y el 8 el primero
    // del sector 1.
    let mut buf = [0u8; 1024];
    disks[0].read_block(7, &mut buf).expect("la lectura va");

    assert_eq!(
        &buf[..512],
        &[8u8; 512][..],
        "la primera mitad ha venido mal"
    );
    assert_eq!(
        &buf[512..],
        &[9u8; 512][..],
        "la segunda mitad ha venido mal"
    );
    assert_eq!(
        hw.commands() - antes,
        2,
        "no ha pedido un sector del aparato por cada mitad"
    );
}

/// Lo que de verdad importa del camino parcial: una escritura de 512 bytes en
/// un disco 4Kn **no puede** someter el sector sin haberlo leído antes, o se
/// lleva por delante los otros 3.584 bytes. Un sistema de ficheros que
/// actualiza un inodo escribe así todo el rato.
#[test]
fn una_escritura_de_512_en_un_disco_4kn_no_pisa_el_resto_del_sector() {
    let (hw, disks) = con_disco_4kn(4);
    hw.fill_disk(0, &[0xee; 4096]);

    disks[0]
        .write_block(2, &[0xab; 512])
        .expect("la escritura va");

    assert_eq!(
        hw.disk_bytes(2 * 512, 512),
        alloc::vec![0xab; 512],
        "los bytes no han llegado a su trozo"
    );
    assert_eq!(
        hw.disk_bytes(0, 2 * 512),
        alloc::vec![0xee; 2 * 512],
        "ha pisado lo que había delante en el mismo sector"
    );
    assert_eq!(
        hw.disk_bytes(3 * 512, 5 * 512),
        alloc::vec![0xee; 5 * 512],
        "ha pisado lo que había detrás en el mismo sector"
    );
}

/// Y se ve en el orden de los comandos: leer y luego escribir. Sin el primero
/// el disco queda consistente con lo que el llamante pidió y mal en todo lo
/// demás, que es un estropicio que ningún `Err` anuncia.
#[test]
fn una_escritura_parcial_lee_el_sector_antes_de_pisarlo() {
    let (hw, disks) = con_disco_4kn(4);
    let antes = hw.opcodes().len();

    disks[0]
        .write_block(5, &[0x5a; 512])
        .expect("la escritura va");

    assert_eq!(
        &hw.opcodes()[antes..],
        &[ATA_CMD_READ_DMA_EXT, ATA_CMD_WRITE_DMA_EXT][..],
        "la lectura-modificación-escritura no está"
    );
}

/// El control del par anterior: un sector del aparato entero y alineado **no**
/// se lee antes. Si se leyera, cada escritura grande costaría el doble de
/// comandos, y es la condición del camino parcial la que lo decide.
#[test]
fn una_escritura_de_un_sector_4kn_entero_y_alineado_no_lo_lee_antes() {
    let (hw, disks) = con_disco_4kn(4);
    let antes = hw.opcodes().len();

    // Bloque 8 = byte 4096 = principio del sector 1 del aparato, y 4096 bytes
    // justos.
    disks[0]
        .write_block(8, &[0x77; 4096])
        .expect("la escritura va");

    assert_eq!(
        &hw.opcodes()[antes..],
        &[ATA_CMD_WRITE_DMA_EXT][..],
        "ha leído un sector que iba a sobrescribir entero"
    );
    assert_eq!(hw.disk_bytes(4096, 4096), alloc::vec![0x77; 4096]);
}

// --- el vaciado de caché que falla ----------------------------------------

/// El vaciado tiene dos opcodes como la lectura y la escritura, y el aparato
/// antiguo no entiende el de 48 bits: contestaría con error en el `fsync` de
/// cada fichero.
#[test]
fn un_disco_sin_lba48_vacia_la_cache_con_el_opcode_antiguo() {
    reloj();
    let mut d = Disk::of_512(64);
    d.lba48 = false;
    let hw = Hw::new(1, Some(d), true);
    let disks = AhciInterface::new_all(hw.base(), 0).expect("enumera");
    let antes = hw.opcodes().len();

    disks[0].flush().expect("el flush va");

    assert_eq!(
        &hw.opcodes()[antes..],
        &[ATA_CMD_FLUSH_CACHE][..],
        "{:?}",
        hw.opcodes()
    );
}

/// Un vaciado que falla una vez se reintenta, y por eso existe: `apk` hace un
/// `fsync` por fichero extraído, y el de los 130 MiB de `libLLVM.so` deja la
/// caché del aparato llena; un solo tropiezo sin reintentar abortaba la
/// extracción entera con EIO mientras los paquetes pequeños entraban todos.
#[test]
fn un_vaciado_que_falla_una_vez_se_reintenta_y_acaba_pasando() {
    let (hw, disks) = con_disco(64);
    let antes = hw.commands();
    hw.faults(Faults {
        tfes_commands: 1,
        ..Default::default()
    });

    disks[0]
        .flush()
        .expect("el flush no ha sobrevivido al tropiezo");

    assert_eq!(
        hw.commands() - antes,
        2,
        "no ha reintentado exactamente una vez"
    );
}

/// Y uno que no deja de fallar se rinde con error en vez de reintentar para
/// siempre: un `fsync` que no vuelve cuelga al proceso que lo pidió.
#[test]
fn un_vaciado_que_no_deja_de_fallar_se_rinde_con_error() {
    let (hw, disks) = con_disco(64);
    let antes = hw.commands();
    hw.faults(Faults {
        tfes_commands: u32::MAX,
        ..Default::default()
    });

    assert_eq!(
        disks[0].flush().err(),
        Some(DeviceError::IoError),
        "un vaciado que nunca pasa ha salido bueno"
    );
    let intentos = hw.commands() - antes;
    assert!(
        (2..=RW_RETRIES).contains(&intentos),
        "se ha rendido sin reintentar o ha reintentado de más: {} comandos",
        intentos
    );
}

// --- un IDENTIFY que no se puede creer ------------------------------------

/// Las palabras 117-118 del IDENTIFY traen el tamaño de sector en palabras, y
/// un aparato puede marcarlas válidas y poner dentro algo imposible. El driver
/// se queda en 512 en vez de creérselo: con un tamaño de sector que no es
/// potencia de dos toda la aritmética de arriba parte los comandos donde no
/// debe, y la alternativa sería no montar el disco.
#[test]
fn un_identify_que_declara_un_sector_imposible_se_queda_en_512() {
    reloj();
    let mut d = Disk::of_512(64);
    // Bit 14 «válido» y bit 12 «pasa de 256 palabras», y luego 1.500 palabras
    // = 3.000 bytes, que no es potencia de dos.
    d.word106 = Some((1 << 14) | (1 << 12));
    d.words117_118 = Some(1_500);
    let hw = Hw::new(1, Some(d), true);
    let disks = AhciInterface::new_all(hw.base(), 0).expect("enumera");

    assert_eq!(
        disks[0].logical_block_size(),
        512,
        "se ha creído un sector de 3.000 bytes"
    );
    // Y el disco sigue sirviendo: la cuenta de bloques es la de 512 bytes.
    assert_eq!(disks[0].block_count(), 64);
    disks[0].read_block(1, &mut [0u8; 512]).expect("lee");
}

// --- lo que no debe tocar nada --------------------------------------------

/// El `handle_irq` de este driver no hace nada a propósito: la espera de cada
/// comando sondea `PxIS` y `PxCI` ella misma, así que un manejador que
/// limpiara `PxIS` se llevaría por delante el aviso que la espera está a punto
/// de leer. Lo que se afirma es que no toca el puerto.
#[test]
fn una_interrupcion_en_el_puerto_no_se_lleva_el_aviso_que_el_comando_espera() {
    let (hw, disks) = con_disco(64);
    hw.poke_port(0, PORT_IS, 0x1);
    let comandos = hw.commands();

    disks[0].handle_irq(11);

    assert_eq!(
        hw.peek_port(0, PORT_IS),
        0x1,
        "el manejador ha limpiado el aviso del puerto"
    );
    assert_eq!(hw.commands(), comandos, "ha sometido un comando");
}

// --- el PRDT con la traducción de verdad ----------------------------------

/// `build_prds` es la única llamada que usa la traducción de direcciones de
/// verdad en vez de una puesta por el test, y la unidad en la que parte es la
/// página de 4 KiB. Aquí la traducción es la identidad, así que un trozo de
/// montón de dos páginas tiene que salir en un solo rango coalescido: si
/// partiera por otra unidad saldrían dos, y si la traducción fallara, `None`.
#[test]
fn el_prdt_de_un_buffer_del_anfitrion_sale_de_la_traduccion_de_verdad() {
    let buf = alloc::vec![0u8; 3 * 4096];
    // Alineado a página dentro del trozo, para que el primer rango empiece en
    // un borde y los dos siguientes se puedan coalescer.
    let va = (buf.as_ptr() as usize + 4095) & !4095;
    let mut prds = [(0u64, 0usize); PRDT_MAX];

    let n = build_prds(va, 2 * 4096, &mut prds).expect("la traducción es la identidad");

    assert_eq!(
        n, 1,
        "dos páginas contiguas no se han coalescido: {:?}",
        prds
    );
    assert_eq!(prds[0], (va as u64, 2 * 4096));
}

// --- a quién reconoce el driver ------------------------------------------

/// El driver se queda con los controladores en modo AHCI, y el modo es el
/// `prog_if`: la misma clase con `prog_if` 0 es un IDE a la antigua, que no
/// tiene ni ABAR ni lista de comandos. Y las tres subclases que acepta son
/// las tres en las que un AHCI se presenta de verdad: SATA, IDE y RAID
/// —muchas BIOS entregan el controlador con el modo RAID puesto, y por
/// debajo sigue siendo un AHCI—.
#[test]
fn el_driver_reconoce_los_controladores_en_modo_ahci() {
    let drv = AhciDriverPci;

    assert_eq!(drv.name(), "ahci");
    // El filtro por fabricante no reconoce a nadie: aquí decide la clase.
    assert!(!drv.matched(0x8086, 0x2922));

    for subclase in [0x06u8, 0x01, 0x04] {
        assert!(
            drv.matched_dev(&dispositivo(0x01, subclase, 0x01)),
            "no reconoce la subclase {:#x} en modo AHCI",
            subclase
        );
    }
    assert!(
        !drv.matched_dev(&dispositivo(0x01, 0x01, 0x00)),
        "ha reconocido un IDE a la antigua"
    );
    assert!(
        !drv.matched_dev(&dispositivo(0x01, 0x08, 0x01)),
        "ha reconocido un NVMe"
    );
    assert!(
        !drv.matched_dev(&dispositivo(0x0c, 0x03, 0x01)),
        "ha reconocido un controlador USB"
    );
    // Y la clase se mira de verdad: un aparato de audio comparte con el AHCI
    // la subclase 0x01 y el `prog_if` 0x01, así que mirar sólo esas dos
    // mitades se lo llevaría por delante.
    assert!(
        !drv.matched_dev(&dispositivo(0x04, 0x01, 0x01)),
        "ha reconocido un aparato de audio"
    );
}

/// Un `PCIDevice` con esa clase, esa subclase y ese `prog_if`, sin BAR.
fn dispositivo(class: u8, subclass: u8, prog_if: u8) -> pci::PCIDevice {
    pci::PCIDevice {
        loc: pci::Location {
            bus: 0,
            device: 31,
            function: 2,
        },
        id: pci::Identifier {
            vendor_id: 0x8086,
            device_id: 0x2922,
            revision_id: 0,
            prog_if,
            class,
            subclass,
        },
        command: pci::Command::empty(),
        status: pci::Status::empty(),
        cache_line_size: 0,
        latency_timer: 0,
        multifunction: false,
        bist_capable: false,
        bars: [None; 6],
        kind: pci::DeviceKind::Device(pci::DeviceDetails {
            cardbus_cis_ptr: 0,
            subsystem_vendor_id: 0,
            subsystem_id: 0,
            expansion_rom_base_addr: 0,
            min_grant: 0,
            max_latency: 0,
        }),
        pic_interrupt_line: 0,
        interrupt_pin: None,
        cspace_access_method: pci::CSpaceAccessMethod::IO,
        capabilities: None,
    }
}
