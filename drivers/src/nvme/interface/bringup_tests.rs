//! El arranque del controlador NVMe: el reset, la cola de administración, lo
//! que el Identify dice del namespace y las colas de entrada y salida.
//!
//! Este driver no tenía un solo test. Lo que se arranca aquí es el camino por
//! el que pasa cada disco NVMe del sistema antes de que exista: si el reset no
//! se espera, si una terminación de otro comando cierra la espera, o si un
//! namespace que el controlador declara a cero se da por bueno, lo que sale
//! por el otro lado es un disco que contesta basura o un arranque colgado.

use super::nvme_fake::{Averias, Hw, SC_INTERNAL};
use super::*;
use crate::utils::host_hooks as shim;

const BAR_LEN: usize = 0x2000;
const IRQ: usize = 11;
/// 2 MiB de namespace: 4096 LBAs de 512 bytes.
const BLOQUES: u64 = 4096;

fn sano() -> (Hw, NvmeInterface) {
    let hw = Hw::new(BLOQUES);
    let dev = NvmeInterface::new(hw.base(), BAR_LEN, IRQ).expect("el controlador arranca");
    (hw, dev)
}

fn con(averias: Averias) -> (Hw, DeviceResult<NvmeInterface>) {
    let hw = Hw::con(averias);
    let r = NvmeInterface::new(hw.base(), BAR_LEN, IRQ);
    (hw, r)
}

fn averias_sanas() -> Averias {
    Averias {
        nsze: BLOQUES,
        lbads: 9,
        mdts: 5,
        ..Default::default()
    }
}

// --- el arranque -----------------------------------------------------------

/// La vuelta normal: el controlador arranca y el disco sale con su capacidad
/// en sectores de 512 bytes, que es la unidad en la que habla el resto del
/// kernel.
#[test]
fn un_controlador_sano_arranca_con_la_capacidad_de_su_namespace() {
    let (_hw, dev) = sano();
    assert_eq!(dev.capacity, BLOQUES as usize);
    assert_eq!(dev.lba_shift, 9);
}

/// Antes de tocar nada hay que quitarle `CC.EN` y esperar a que baje
/// `CSTS.RDY`: programar la cola de administración con el controlador
/// corriendo es escribirle los registros por debajo.
#[test]
fn el_arranque_apaga_el_controlador_antes_de_programarlo() {
    let (hw, _dev) = sano();
    let ccs = hw.cc_writes();
    assert!(ccs.len() >= 2, "sólo ha escrito CC {} vez", ccs.len());
    assert_eq!(
        ccs[0] & NVME_CC_ENABLE,
        0,
        "la primera escritura de CC no lo apaga"
    );
    assert_ne!(
        ccs[1] & NVME_CC_ENABLE,
        0,
        "la segunda escritura de CC no lo vuelve a encender"
    );
}

/// La cola de administración se declara con su tamaño menos uno en las dos
/// mitades de AQA: declararla una entrada más grande es darle al controlador
/// una entrada que no existe.
#[test]
fn la_cola_de_administracion_se_declara_en_base_cero() {
    let (hw, _dev) = sano();
    let aqa = hw.aqa();
    assert_eq!(aqa & 0xfff, 31, "el tamaño de la cola de envío");
    assert_eq!((aqa >> 16) & 0xfff, 31, "el de la de terminación");
}

/// El driver sondea, no espera interrupciones, así que las enmascara todas.
#[test]
fn las_interrupciones_del_controlador_se_enmascaran() {
    let (hw, _dev) = sano();
    assert_eq!(hw.intms(), 0xffff_ffff);
}

/// El orden importa: los dos Identify, luego el número de colas, y sólo
/// entonces crear la de terminación y la de envío. Crear una cola de envío
/// antes que su cola de terminación es un error del controlador.
#[test]
fn las_colas_se_crean_en_el_orden_que_manda_la_norma() {
    let (hw, _dev) = sano();
    let ops: alloc::vec::Vec<u8> = hw.vistos().into_iter().map(|(_, op)| op).collect();
    assert_eq!(
        ops,
        alloc::vec![0x06, 0x06, 0x09, 0x05, 0x01],
        "los comandos del arranque no son los que manda la norma"
    );
}

/// Las dos colas de entrada y salida se declaran igual que la de
/// administración, con su tamaño menos uno: declarar una entrada más es
/// darle al controlador una entrada que no existe, y el tamaño de la de
/// envío no es el de la de terminación por casualidad.
#[test]
fn las_colas_de_entrada_y_salida_se_declaran_en_base_cero() {
    let (hw, dev) = sano();
    let io = dev.io_queues[0].lock();
    let (sq, cq) = hw.cola_len(1);
    assert_eq!(sq, io.sq.len(), "el tamaño declarado de la cola de envío");
    assert_eq!(
        cq,
        io.cq.len(),
        "el tamaño declarado de la cola de terminación"
    );
}

/// Cada entrada de terminación que el driver consume se confirma en el
/// timbre de la cola: sin eso el controlador cree que el anfitrión no ha
/// leído nada y acaba sin huecos donde contestar.
#[test]
fn cada_terminacion_consumida_se_confirma_en_el_timbre() {
    let (hw, _dev) = sano();
    let timbres = hw.cq_timbres(0);
    let comandos = hw.vistos().len();
    assert_eq!(
        timbres.len(),
        comandos,
        "{} comandos y {} confirmaciones",
        comandos,
        timbres.len()
    );
    assert_eq!(
        timbres.as_slice(),
        (1..=comandos).collect::<alloc::vec::Vec<_>>().as_slice(),
        "las cabezas confirmadas no van una a una"
    );
}

/// Al dar la vuelta a la cola de terminación el controlador invierte la fase
/// de las entradas nuevas, y el driver tiene que invertir la suya: si no, la
/// vuelta siguiente le parece una cola vacía y se queda esperando.
#[test]
fn la_cola_de_terminacion_da_la_vuelta_sin_perder_la_fase() {
    let (hw, dev) = sano();
    let vueltas = {
        let io = dev.io_queues[0].lock();
        io.cq.len() + 3
    };
    hw.set_sector(1, &marco(0x77, 512));

    let mut buf = alloc::vec![0u8; 512];
    for i in 0..vueltas {
        dev.read_block(1, &mut buf)
            .unwrap_or_else(|e| panic!("la lectura {} de {} falló: {:?}", i, vueltas, e));
        assert_eq!(buf, marco(0x77, 512), "la lectura {} trajo otra cosa", i);
    }
}

// --- los controladores que no arrancan -------------------------------------

/// Un controlador que no baja `CSTS.RDY` cuando le quitan `CC.EN` está
/// colgado: seguir adelante es programarle los registros a un controlador que
/// todavía está usando los de antes.
#[test]
fn un_controlador_que_no_se_resetea_se_abandona() {
    let (_hw, r) = con(Averias {
        never_resets: true,
        ..averias_sanas()
    });
    assert_eq!(r.err(), Some(DeviceError::IoError));
}

/// Y uno que nunca levanta `RDY` tras habilitarlo tampoco va a contestar un
/// comando: el arranque no se puede quedar ahí.
#[test]
fn un_controlador_que_nunca_queda_listo_se_abandona() {
    let (_hw, r) = con(Averias {
        never_ready: true,
        ..averias_sanas()
    });
    assert_eq!(r.err(), Some(DeviceError::IoError));
}

/// `CSTS.CFS` es el controlador diciendo que se ha roto. Esperar a que
/// levante `RDY` es gastar el plazo entero para llegar a la misma conclusión.
#[test]
fn un_estado_fatal_al_habilitar_se_abandona_sin_esperar_el_plazo() {
    let (_hw, r) = con(Averias {
        fatal_on_enable: true,
        ..averias_sanas()
    });
    assert_eq!(r.err(), Some(DeviceError::IoError));
    // El reloj de test avanza un microsegundo cada mil en cada lectura, y el
    // plazo de este controlador (CAP.TO 2) es un segundo: si la rama de
    // `CSTS.CFS` no estuviera, la misma conclusión costaría el plazo entero.
    let gastado = crate::nvme::nvme_queue::test_clock::now();
    assert!(
        gastado < 100_000,
        "ha gastado {} us: no ha mirado CSTS.CFS, ha esperado el plazo",
        gastado
    );
}

/// Un controlador que queda listo pero no contesta ningún comando: el
/// arranque tiene que vencer, no girar para siempre.
#[test]
fn un_controlador_mudo_no_cuelga_el_arranque() {
    let (_hw, r) = con(Averias {
        deaf: true,
        ..averias_sanas()
    });
    assert_eq!(r.err(), Some(DeviceError::IoError));
}

/// Un comando que vuelve con error no se reintenta en el arranque: lo que
/// sale es un disco que no existe, que es mejor que uno a medio programar.
#[test]
fn un_comando_del_arranque_que_falla_aborta_el_arranque() {
    let (_hw, r) = con(Averias {
        status: SC_INTERNAL,
        ..averias_sanas()
    });
    assert_eq!(r.err(), Some(DeviceError::IoError));
}

// --- lo que dice el namespace ----------------------------------------------

/// Un namespace de cero LBAs no se puede usar, y darlo por bueno es un disco
/// de capacidad cero por el que alguien va a leer.
#[test]
fn un_namespace_de_tamano_cero_no_se_usa() {
    let (_hw, r) = con(Averias {
        nsze: 0,
        ..averias_sanas()
    });
    assert_eq!(r.err(), Some(DeviceError::NoResources));
}

/// Un tamaño de LBA que el driver no soporta se rechaza: el camino de
/// actualización parcial da por hecho que un LBA cabe de sobra en el búfer.
#[test]
fn un_tamano_de_lba_que_no_se_soporta_se_rechaza() {
    let (_hw, r) = con(Averias {
        lbads: 20,
        ..averias_sanas()
    });
    assert_eq!(r.err(), Some(DeviceError::NotSupported));
}

/// La capacidad se cuenta en sectores de 512 bytes, así que un namespace de
/// LBAs de 4 KiB tiene ocho veces más sectores que LBAs.
#[test]
fn un_namespace_de_lbas_de_cuatro_kib_cuenta_sus_sectores_de_512() {
    let hw = Hw::con(Averias {
        lbads: 12,
        ..averias_sanas()
    });
    let dev = NvmeInterface::new(hw.base(), BAR_LEN, IRQ).expect("el controlador arranca");
    assert_eq!(dev.lba_shift, 12);
    assert_eq!(dev.capacity, BLOQUES as usize * 8);
}

/// MDTS es el techo de lo que cabe en un comando, en páginas de 4 KiB y como
/// potencia de dos. Pasarse de ahí es un comando que el controlador rechaza.
#[test]
fn el_techo_de_transferencia_sale_del_mdts() {
    let hw = Hw::con(Averias {
        mdts: 3,
        ..averias_sanas()
    });
    let dev = NvmeInterface::new(hw.base(), BAR_LEN, IRQ).expect("el controlador arranca");
    assert_eq!(dev.max_transfer, 8 * 4096);
}

// --- la espera de la terminación -------------------------------------------

/// Una terminación de otro comando se consume y se sigue esperando la
/// nuestra: abandonarla ahí deja la cola desincronizada con el controlador y
/// el disco muerto a partir de ese momento.
#[test]
fn una_terminacion_de_otro_comando_no_cierra_la_espera() {
    let hw = Hw::con(Averias {
        stale: 1,
        ..averias_sanas()
    });
    let dev = NvmeInterface::new(hw.base(), BAR_LEN, IRQ).expect("el controlador arranca igual");
    assert_eq!(dev.capacity, BLOQUES as usize);
}

// --- la ventana de registros -----------------------------------------------

/// Los timbres viven a `stride` unos de otros a partir de 0x1000, y el
/// último tiene que caber en la BAR: el driver lo escribe en cada comando.
#[test]
fn un_stride_de_timbres_que_no_cabe_en_la_bar_se_rechaza() {
    let hw = Hw::con(averias_sanas());
    // DSTRD 15: timbres de 128 KiB en una ventana de 8 KiB.
    hw.set_cap(31 | (15u64 << 32) | (2u64 << 24));
    assert_eq!(
        NvmeInterface::new(hw.base(), BAR_LEN, IRQ).err(),
        Some(DeviceError::NotSupported)
    );
}

/// Una cola de una sola entrada está llena en cuanto hay algo en ella, así
/// que no se llegaría a enviar ni un comando.
#[test]
fn un_controlador_con_una_cola_de_una_entrada_se_rechaza() {
    let hw = Hw::con(averias_sanas());
    hw.set_cap(0 | (2u64 << 24));
    assert_eq!(
        NvmeInterface::new(hw.base(), BAR_LEN, IRQ).err(),
        Some(DeviceError::NotSupported)
    );
}

/// Y un arranque que falla no se puede dejar la memoria de las colas por el
/// camino: cada disco que no enumera se la llevaría.
#[test]
fn un_arranque_que_falla_devuelve_la_memoria_de_sus_colas() {
    shim::alone_with_the_allocator(|| {
        let (_hw, r) = con(Averias {
            never_ready: true,
            ..averias_sanas()
        });
        assert!(r.is_err());
        assert_eq!(
            shim::ALLOC_PAGES.load(core::sync::atomic::Ordering::SeqCst),
            shim::DEALLOC_PAGES.load(core::sync::atomic::Ordering::SeqCst),
            "el arranque fallido se ha dejado paginas sin devolver"
        );
    });
}

// --- leer y escribir -------------------------------------------------------

fn marco(semilla: u8, len: usize) -> alloc::vec::Vec<u8> {
    (0..len).map(|i| semilla ^ (i as u8)).collect()
}

/// La vuelta normal de una lectura: lo que hay en el medio sale por el búfer
/// del llamante, byte a byte.
#[test]
fn un_sector_del_medio_se_lee_entero() {
    let (hw, dev) = sano();
    let datos = marco(0x5a, 512);
    hw.set_sector(7, &datos);

    let mut buf = alloc::vec![0u8; 512];
    dev.read_block(7, &mut buf).expect("la lectura va");

    assert_eq!(buf, datos);
}

/// Y la de una escritura: lo que el llamante dio acaba en el medio, en su
/// LBA y no en otro.
#[test]
fn un_sector_escrito_acaba_en_su_lba() {
    let (hw, dev) = sano();
    let datos = marco(0x3c, 512);

    dev.write_block(9, &datos).expect("la escritura va");

    assert_eq!(hw.sector(9), datos);
    assert_eq!(
        hw.sector(8),
        alloc::vec![0u8; 512],
        "ha pisado el de al lado"
    );
}

/// Varios sectores de una vez van en un solo comando mientras quepan, y
/// tienen que salir en orden: una lista de PRP mal montada se nota aquí.
#[test]
fn varios_sectores_seguidos_se_leen_en_orden() {
    let (hw, dev) = sano();
    for lba in 0..16u64 {
        hw.set_sector(lba, &marco(lba as u8, 512));
    }

    let mut buf = alloc::vec![0u8; 16 * 512];
    dev.read_block(0, &mut buf).expect("la lectura va");

    for lba in 0..16usize {
        assert_eq!(
            &buf[lba * 512..(lba + 1) * 512],
            marco(lba as u8, 512).as_slice(),
            "el sector {lba} no está en su sitio"
        );
    }
}

/// Una petición que se sale de la capacidad se rechaza antes de mandar nada:
/// el controlador contestaría error, pero el disco ya estaría con un comando
/// en vuelo por un LBA que no existe.
#[test]
fn una_peticion_fuera_de_la_capacidad_no_llega_al_controlador() {
    let (hw, dev) = sano();
    let antes = hw.vistos().len();
    let mut buf = alloc::vec![0u8; 512];

    assert!(dev.read_block(BLOQUES as usize, &mut buf).is_err());

    assert_eq!(hw.vistos().len(), antes, "ha mandado el comando igual");
}

/// Un comando de entrada y salida que vuelve con error se reintenta: un
/// hipo de los que pasan no puede tumbar una descompresión entera.
#[test]
fn una_lectura_que_falla_se_reintenta() {
    let (hw, dev) = sano();
    let antes = hw.vistos().len();
    hw.averiar(|a| a.status = SC_INTERNAL);

    let mut buf = alloc::vec![0u8; 512];
    assert!(dev.read_block(0, &mut buf).is_err());

    assert!(
        hw.vistos().len() - antes > 1,
        "no ha reintentado: sólo mandó {} comando",
        hw.vistos().len() - antes
    );
}
