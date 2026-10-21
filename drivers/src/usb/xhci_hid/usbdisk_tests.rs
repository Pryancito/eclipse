//! El disco que el sistema ve: `drain_disk_changes` publicándolo y `UsbDisk`
//! contestando lecturas y escrituras por el camino de bloques.
//!
//! `msc_tests` llega hasta `msc_rw`, que habla en bloques *del dispositivo*.
//! Lo que falta de ahí al sistema de archivos son dos cosas que no se ven
//! desde ahí dentro: que el disco se dé de alta y de baja por el hueco de
//! `hotplug` (y con la MISMA `Arc`, porque la baja es por identidad), y que
//! `block_id`, que viene en sectores de 512, se traduzca al bloque de la
//! unidad. Con bloques de 512 esa traducción es la identidad y no se nota;
//! con los de 4096 de un disco externo, pedirle «el bloque 7» cuando el
//! kernel dijo «el sector 7» lee del sitio equivocado.
//!
//! Estos tests instalan destino en `hotplug`, que es un global del proceso,
//! así que toman turno en `hotplug::TEST_TURNSTILE` con los del propio
//! `hotplug`.

use super::xhci_fake::{FakeDev, Hw};
use super::*;
use alloc::string::{String, ToString};

const SLOTS: u8 = 8;
const PORTS: u8 = 4;
const HS: u8 = 3;

/// 64 bloques de 512: un pendrive de juguete con la aritmética de uno de
/// verdad.
const BLOCKS: u64 = 64;
const BS: u32 = 512;

/// Lo que llegó al destino, sin retirar. El destino es un puntero a función
/// plana y no puede capturar nada, así que la contabilidad es global y la
/// limpia el guardia.
static ADDED: Mutex<Vec<Device>> = Mutex::new(Vec::new());
/// Los nombres de los que se retiraron, en orden.
static REMOVED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Puesto a `true`, el destino se lleva la unidad por delante justo mientras
/// el disco se da de alta. Ver `un_disco_cuya_unidad_se_va_al_darla_de_alta`.
static ROBA_LA_UNIDAD: AtomicBool = AtomicBool::new(false);
/// El controlador, para que el destino pueda tocarlo: el destino es un
/// puntero a función plana y no puede capturar nada.
static CTRL: Mutex<Option<Arc<XhciUsbHid>>> = Mutex::new(None);

fn sink_add(dev: Device) {
    ADDED.lock().push(dev);
    if !ROBA_LA_UNIDAD.load(Ordering::Relaxed) {
        return;
    }
    // El alta ocurre con el cerrojo del controlador SUELTO (encadenarlo con
    // el de las listas del sistema es la forma de montar un ciclo), así que
    // aquí dentro la unidad se puede haber ido ya.
    if let Some(c) = CTRL.lock().as_ref() {
        if let Some(xi) = c.inner.lock().as_mut() {
            xi.mscs.clear();
        }
    }
}

/// Un destino que retira **por identidad**, como el de `kernel-hal`: una
/// copia equivalente del mismo disco no vale para quitarlo.
fn sink_remove(dev: &Device) -> bool {
    let mut g = ADDED.lock();
    let Some(i) = g.iter().position(|d| same_device(d, dev)) else {
        return false;
    };
    REMOVED.lock().push(g[i].inner().name().to_string());
    g.remove(i);
    true
}

fn same_device(a: &Device, b: &Device) -> bool {
    match (a, b) {
        (Device::Block(x), Device::Block(y)) => Arc::ptr_eq(x, y),
        _ => false,
    }
}

/// Deja el hueco de `hotplug` y la contabilidad limpios a los dos lados del
/// test, y toma turno mientras dura.
struct Alone(#[allow(dead_code)] lock::MutexGuard<'static, ()>);

impl Alone {
    /// Con destino instalado: el caso normal.
    fn new() -> Self {
        let a = Self::bare();
        crate::hotplug::set_sink(sink_add, sink_remove);
        a
    }

    /// Sin destino: antes de que `kernel-hal` rellene el hueco.
    fn bare() -> Self {
        let g = crate::hotplug::TEST_TURNSTILE.lock();
        clear();
        Self(g)
    }
}

impl Drop for Alone {
    fn drop(&mut self) {
        clear();
    }
}

fn clear() {
    crate::hotplug::clear_sink();
    ROBA_LA_UNIDAD.store(false, Ordering::Relaxed);
    *CTRL.lock() = None;
    ADDED.lock().clear();
    REMOVED.lock().clear();
}

fn ctrl(x: XhciInner) -> Arc<XhciUsbHid> {
    Arc::new(XhciUsbHid {
        listener: EventListener::new(),
        inner: Mutex::new(Some(x)),
        msi_vector: 0,
        halted: AtomicBool::new(false),
        has_rel_mouse_flag: AtomicBool::new(false),
        has_tablet_flag: AtomicBool::new(false),
        // El reconocimiento sin cerrojo no entra aquí; con las bases a cero
        // se va de vuelta sin tocar nada.
        ack_op_base: AtomicUsize::new(0),
        ack_rt_base: AtomicUsize::new(0),
    })
}

/// Un controlador arrancado con esa unidad enumerada en el puerto 1, ya
/// envuelto en su `Arc`, que es lo que necesita el alta.
fn with_disk(dev: FakeDev) -> (Hw, Arc<XhciUsbHid>) {
    let hw = Hw::new(SLOTS, PORTS);
    hw.set_auto_advance(1_000);
    let mut x = hw.inner(SLOTS, PORTS, 0);
    x.reset_and_run().expect("el controlador arranca");
    hw.plug(1, HS, dev);
    x.setup_device(DevTopo::root(1), HS)
        .expect("la unidad enumera");
    (hw, ctrl(x))
}

/// El único disco publicado, por su interfaz de bloques, que es lo que ve el
/// sistema de archivos.
fn disk() -> Arc<dyn BlockScheme> {
    let g = ADDED.lock();
    assert_eq!(g.len(), 1, "no hay exactamente un disco publicado");
    match &g[0] {
        Device::Block(b) => b.clone(),
        _ => panic!("lo publicado no es un dispositivo de bloques"),
    }
}

/// Los códigos de orden que recibió la unidad, en orden.
fn ops(hw: &Hw) -> Vec<u8> {
    hw.scsi_cdbs(1)
        .iter()
        .filter_map(|c| c.first().copied())
        .collect()
}

// --- el alta y la baja ----------------------------------------------------

/// El camino entero de un pendrive: se enumera, contesta su capacidad, y el
/// vaciado de colas lo convierte en un dispositivo de bloques del sistema.
/// Lo que se publica cuenta en **sectores de 512**, que es la unidad en la
/// que el kernel pide bloques.
#[test]
fn un_pendrive_enchufado_se_publica_como_dispositivo_de_bloques() {
    let _alone = Alone::new();
    let (_hw, dev) = with_disk(FakeDev::disk(BLOCKS, BS));

    drain_disk_changes(&dev);

    let d = disk();
    assert_eq!(d.block_count(), BLOCKS as usize, "no cuenta sus sectores");
    assert_eq!(d.logical_block_size(), 512);
    assert!(
        d.name().starts_with("usbdisk"),
        "nombre inesperado: {}",
        d.name()
    );
    // Y el controlador se queda con la misma `Arc`: sin eso, el desenchufe no
    // tiene qué retirar.
    let g = dev.inner.lock();
    let m = &g.as_ref().expect("el controlador sigue").mscs[0];
    assert!(m.disk.is_some(), "el disco no quedó anotado en la unidad");
}

/// Vaciar las colas dos veces no publica el disco dos veces: la cola se
/// consume, y un barrido periódico la visita muchas más veces que enchufes
/// hay.
#[test]
fn vaciar_las_colas_otra_vez_no_publica_el_disco_dos_veces() {
    let _alone = Alone::new();
    let (_hw, dev) = with_disk(FakeDev::disk(BLOCKS, BS));

    drain_disk_changes(&dev);
    drain_disk_changes(&dev);

    assert_eq!(ADDED.lock().len(), 1, "el disco se publicó dos veces");
}

/// Antes de que `kernel-hal` rellene el hueco no hay dónde poner un disco.
/// Lo que no puede pasar es que el controlador se lo apunte como dado de
/// alta: nadie le va a pedir lecturas, y el desenchufe intentaría retirar
/// algo que nunca entró.
#[test]
fn sin_destino_instalado_el_disco_no_se_apunta_como_dado_de_alta() {
    let _alone = Alone::bare();
    let (_hw, dev) = with_disk(FakeDev::disk(BLOCKS, BS));

    drain_disk_changes(&dev);

    assert!(
        ADDED.lock().is_empty(),
        "ha llegado a un destino inexistente"
    );
    let g = dev.inner.lock();
    let m = &g.as_ref().expect("el controlador sigue").mscs[0];
    assert!(m.disk.is_none(), "se apuntó un disco que se descartó");
}

/// Una unidad sin capacidad no es un disco: un lector de tarjetas vacío
/// contesta el INQUIRY y nada más, y publicarlo pondría en el sistema un
/// disco de cero sectores que falla cada lectura.
#[test]
fn una_unidad_que_nunca_esta_lista_no_se_publica() {
    let _alone = Alone::new();
    let (_hw, dev) = with_disk(FakeDev::disk(BLOCKS, BS).with_not_ready(u32::MAX));

    drain_disk_changes(&dev);

    assert!(
        ADDED.lock().is_empty(),
        "se publicó una unidad sin capacidad"
    );
}

/// Una unidad que contesta quién es pero no su capacidad tampoco es un
/// disco. Es un caso distinto del anterior: aquí la unidad está lista y el
/// INQUIRY sale bien, así que la identificación llega hasta el final; lo que
/// falta es la capacidad, y publicarla pondría en el sistema un disco de un
/// sector que falla cada lectura de más allá.
#[test]
fn una_unidad_que_no_contesta_su_capacidad_no_se_publica() {
    let _alone = Alone::new();
    let (_hw, dev) = with_disk(FakeDev::disk(BLOCKS, BS).failing_scsi(SCSI_READ_CAPACITY_10));

    drain_disk_changes(&dev);

    assert!(
        dev.inner
            .lock()
            .as_ref()
            .expect("el controlador sigue")
            .mscs[0]
            .capacity
            .is_none(),
        "la unidad se quedó con una capacidad que nunca contestó"
    );
    assert!(
        ADDED.lock().is_empty(),
        "se publicó una unidad sin capacidad"
    );
}

/// Y una que contesta la capacidad pero con bloques de cero bytes: la orden
/// va bien y lo que dice no se puede usar. Tomarlo por bueno dividiría por
/// cero en la primera traducción de sectores.
#[test]
fn una_unidad_que_dice_bloques_de_cero_no_se_publica() {
    let _alone = Alone::new();
    let (hw, dev) = with_disk(FakeDev::disk(BLOCKS, BS).with_zero_block_size());

    // La cola de altas es lo que mira el vaciado: una unidad que no tiene
    // capacidad no llega siquiera a entrar en ella.
    assert!(
        dev.inner
            .lock()
            .as_ref()
            .expect("el controlador sigue")
            .pending_disk_regs
            .is_empty(),
        "se encoló para publicarla una unidad sin capacidad"
    );
    drain_disk_changes(&dev);

    assert!(
        ops(&hw).contains(&SCSI_READ_CAPACITY_10),
        "ni se preguntó la capacidad: {:?}",
        ops(&hw)
    );
    assert!(
        ADDED.lock().is_empty(),
        "se publicó un disco con bloques de cero bytes"
    );
}

/// La carrera que el alta tiene que atrapar: el disco se da de alta con el
/// cerrojo del controlador suelto, así que la unidad puede haberse ido entre
/// que se publica y que se anota. Un disco dado de alta y sin anotar no lo
/// retira ningún desenchufe: se queda en el sistema para siempre, y sus
/// lecturas caerían en lo siguiente que ocupe ese slot.
#[test]
fn un_disco_cuya_unidad_se_va_al_darla_de_alta_se_retira() {
    let _alone = Alone::new();
    let (_hw, dev) = with_disk(FakeDev::disk(BLOCKS, BS));
    *CTRL.lock() = Some(dev.clone());
    ROBA_LA_UNIDAD.store(true, Ordering::Relaxed);

    drain_disk_changes(&dev);

    assert_eq!(ADDED.lock().len(), 0, "el disco se quedó dado de alta");
    assert_eq!(REMOVED.lock().len(), 1, "no se retiró el disco huérfano");
}

/// Dos pendrives son dos discos, cada uno con su nombre: el nombre sale del
/// identificador de la unidad, no del puerto ni del slot, que se reasignan.
#[test]
fn dos_unidades_se_publican_cada_una_con_su_nombre() {
    let _alone = Alone::new();
    let hw = Hw::new(SLOTS, PORTS);
    hw.set_auto_advance(1_000);
    let mut x = hw.inner(SLOTS, PORTS, 0);
    x.reset_and_run().expect("el controlador arranca");
    hw.plug(1, HS, FakeDev::disk(BLOCKS, BS));
    hw.plug(2, HS, FakeDev::disk(BLOCKS, BS));
    x.setup_device(DevTopo::root(1), HS).expect("la primera");
    x.setup_device(DevTopo::root(2), HS).expect("la segunda");
    let dev = ctrl(x);

    drain_disk_changes(&dev);

    let g = ADDED.lock();
    assert_eq!(g.len(), 2, "no se publicaron los dos discos");
    let nombres: Vec<String> = g.iter().map(|d| d.inner().name().to_string()).collect();
    assert_ne!(nombres[0], nombres[1], "los dos discos comparten nombre");
}

/// Y el desenchufe: el disco tiene que salir del sistema. Dejarlo dado de
/// alta es peor que cosmético, porque el slot se reasigna a lo siguiente que
/// enchufen y una lectura por la entrada vieja caería en otro dispositivo.
#[test]
fn un_pendrive_desenchufado_se_da_de_baja() {
    let _alone = Alone::new();
    let (hw, dev) = with_disk(FakeDev::disk(BLOCKS, BS));
    drain_disk_changes(&dev);
    let nombre = disk().name().to_string();

    hw.unplug(1);
    dev.inner
        .lock()
        .as_mut()
        .expect("el controlador sigue")
        .handle_port_status_change(1)
        .expect("el cambio de puerto se atiende");
    drain_disk_changes(&dev);

    assert!(ADDED.lock().is_empty(), "el disco sigue dado de alta");
    assert_eq!(
        *REMOVED.lock(),
        alloc::vec![nombre],
        "no se retiró por identidad"
    );
}

// --- la traducción de sectores -------------------------------------------

/// Lo mínimo del camino de bloques: un sector pedido por el sistema llega a
/// la unidad y vuelve con lo que había en el medio.
#[test]
fn una_lectura_de_un_sector_trae_lo_que_hay_en_el_medio() {
    let _alone = Alone::new();
    let (_hw, dev) = with_disk(FakeDev::disk(BLOCKS, BS).with_block(3, &[0xab; 512]));
    drain_disk_changes(&dev);

    let mut buf = [0u8; 512];
    disk().read_block(3, &mut buf).expect("la lectura va");

    assert_eq!(buf, [0xab; 512], "el sector 3 no trajo su contenido");
}

/// Y una escritura llega al medio de verdad, en el sector que se pidió.
#[test]
fn una_escritura_de_un_sector_llega_al_medio() {
    let _alone = Alone::new();
    let (hw, dev) = with_disk(FakeDev::disk(BLOCKS, BS));
    drain_disk_changes(&dev);

    disk()
        .write_block(5, &[0x5a; 512])
        .expect("la escritura va");

    assert_eq!(
        hw.media_block(1, 5),
        alloc::vec![0x5a; 512],
        "el sector 5 no se escribió en el medio"
    );
    assert_eq!(
        hw.media_block(1, 4),
        alloc::vec![0u8; 512],
        "se escribió en el sector de al lado"
    );
}

/// Un disco externo de bloques de 4096 cuenta **ocho** sectores de 512 por
/// bloque. Si publicara sus bloques como sectores, el sistema vería un disco
/// ocho veces más pequeño y no podría leer sus últimas siete octavas partes.
#[test]
fn un_disco_de_4096_cuenta_sus_sectores_de_512() {
    let _alone = Alone::new();
    let (_hw, dev) = with_disk(FakeDev::disk(8, 4096));
    drain_disk_changes(&dev);

    let d = disk();
    assert_eq!(
        d.block_count(),
        64,
        "no traduce la cuenta a sectores de 512"
    );
    assert_eq!(d.logical_block_size(), 4096);
}

/// La traducción de verdad: el sector 9 de un disco de 4096 es el segundo
/// octavo del bloque 1. Pedirle a la unidad «el bloque 9» leería 4 KiB del
/// sitio equivocado, y el sistema de archivos vería basura.
#[test]
fn un_sector_de_512_en_un_disco_de_4096_sale_de_su_octavo() {
    let _alone = Alone::new();
    // El bloque 1 lleva en cada octavo el número de ese octavo.
    let mut bloque = [0u8; 4096];
    for (i, trozo) in bloque.chunks_mut(512).enumerate() {
        trozo.fill(i as u8);
    }
    let (_hw, dev) = with_disk(FakeDev::disk(8, 4096).with_block(1, &bloque));
    drain_disk_changes(&dev);

    let mut buf = [0u8; 512];
    disk().read_block(9, &mut buf).expect("la lectura va");

    assert_eq!(
        buf, [1u8; 512],
        "el sector 9 no es el octavo 1 del bloque 1"
    );
}

/// Y la escritura de un sector dentro de un bloque más grande es leer,
/// modificar y escribir: escribir el bloque entero con el resto a ceros se
/// lleva por delante los sectores vecinos, que en una tabla de particiones
/// son la tabla.
#[test]
fn una_escritura_de_512_en_un_disco_de_4096_no_toca_los_vecinos() {
    let _alone = Alone::new();
    let mut bloque = [0u8; 4096];
    for (i, trozo) in bloque.chunks_mut(512).enumerate() {
        trozo.fill(0x10 + i as u8);
    }
    let (hw, dev) = with_disk(FakeDev::disk(8, 4096).with_block(1, &bloque));
    drain_disk_changes(&dev);

    disk()
        .write_block(9, &[0xcc; 512])
        .expect("la escritura va");

    let despues = hw.media_block(1, 1);
    assert_eq!(
        &despues[512..1024],
        &[0xcc; 512],
        "no se escribió el sector"
    );
    for (i, trozo) in despues.chunks(512).enumerate() {
        if i == 1 {
            continue;
        }
        assert_eq!(
            trozo,
            &[0x10 + i as u8; 512],
            "el octavo {} del bloque se perdió",
            i
        );
    }
}

/// Una petición que no cae en sectores enteros no se puede traducir, y se
/// rechaza **sin mandar nada**: a medio camino, una escritura habría dejado
/// el medio a medias.
#[test]
fn una_peticion_que_no_es_multiplo_del_sector_se_rechaza_sin_mandar_nada() {
    let _alone = Alone::new();
    let (hw, dev) = with_disk(FakeDev::disk(BLOCKS, BS));
    drain_disk_changes(&dev);
    let antes = ops(&hw).len();
    let d = disk();

    let mut corto = [0u8; 100];
    assert_eq!(
        d.read_block(0, &mut corto),
        Err(DeviceError::InvalidParam),
        "aceptó una lectura de 100 bytes"
    );
    assert_eq!(
        d.write_block(0, &[0u8; 100]),
        Err(DeviceError::InvalidParam),
        "aceptó una escritura de 100 bytes"
    );
    assert_eq!(
        d.read_block(0, &mut []),
        Err(DeviceError::InvalidParam),
        "aceptó una lectura vacía"
    );
    assert_eq!(
        ops(&hw).len(),
        antes,
        "llegó una orden SCSI de una petición rechazada"
    );
}

/// Un `flush` del sistema de archivos tiene que llegar a la unidad como
/// SYNCHRONIZE CACHE: si no, lo escrito se queda en la caché del pendrive y
/// desenchufarlo lo pierde.
#[test]
fn un_flush_llega_como_synchronize_cache() {
    let _alone = Alone::new();
    let (hw, dev) = with_disk(FakeDev::disk(BLOCKS, BS));
    drain_disk_changes(&dev);

    disk().flush().expect("el flush va");

    assert!(
        ops(&hw).contains(&0x35),
        "no se mandó SYNCHRONIZE CACHE: {:?}",
        ops(&hw)
    );
}

/// El disco lo tiene el sistema de archivos y puede sobrevivir al
/// controlador: la referencia al controlador es débil a propósito (un `Arc`
/// sería un ciclo). Cuando el controlador se va, las lecturas tienen que
/// decir que no está listo, no reventar.
#[test]
fn un_disco_cuyo_controlador_se_fue_dice_que_no_esta_listo() {
    let _alone = Alone::new();
    let (_hw, dev) = with_disk(FakeDev::disk(BLOCKS, BS));
    drain_disk_changes(&dev);
    let d = disk();

    ADDED.lock().clear();
    drop(dev);

    let mut buf = [0u8; 512];
    assert_eq!(d.read_block(0, &mut buf), Err(DeviceError::NotReady));
    assert_eq!(d.write_block(0, &[0u8; 512]), Err(DeviceError::NotReady));
    assert_eq!(d.flush(), Err(DeviceError::NotReady));
    // Y la cuenta de sectores sigue contestando: es una foto, no una consulta
    // al aparato, y un sistema de archivos la pide al desmontar.
    assert_eq!(d.block_count(), BLOCKS as usize);
}

/// Una petición de varios sectores se pide a la unidad **de una vez**: el
/// rango abarca todos los bloques que toca, no sólo el primero. Pedir uno y
/// copiar el resto de un búfer medio vacío devolvería ceros donde hay datos.
#[test]
fn una_peticion_de_varios_sectores_cubre_todos_sus_bloques() {
    let _alone = Alone::new();
    let dev = FakeDev::disk(BLOCKS, BS)
        .with_block(10, &[0xa0; 512])
        .with_block(11, &[0xa1; 512])
        .with_block(12, &[0xa2; 512])
        .with_block(13, &[0xa3; 512]);
    let (_hw, ctrl) = with_disk(dev);
    drain_disk_changes(&ctrl);

    let mut buf = [0u8; 2048];
    disk().read_block(10, &mut buf).expect("la lectura va");

    for (i, trozo) in buf.chunks(512).enumerate() {
        assert_eq!(
            trozo,
            &[0xa0 + i as u8; 512],
            "el sector {} de la tanda no trajo lo suyo",
            10 + i
        );
    }
}
