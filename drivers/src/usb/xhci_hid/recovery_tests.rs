//! Cuando el controlador se atraganta: la recuperación suave y el sondeo de
//! respaldo que la llama.
//!
//! Un halt latchado para siempre es el síntoma de «el teclado se muere a los
//! pocos segundos»: la entrada queda muerta hasta reiniciar. Pero insistir
//! eternamente contra un aparato que se ha ido del bus es gastar cada tick
//! del timer en leer registros que contestan a unos. Entre esas dos cosas
//! está este camino, y lo que estos tests fijan es dónde está la raya.

use super::xhci_fake::{FakeDev, Hw};
use super::*;

const SLOTS: u8 = 8;
const PORTS: u8 = 4;
const HS: u8 = 3;

/// `POLL_INSTANCES` es un global del proceso, y el fake es por hilo: dos
/// tests sondeando a la vez se encontrarían con el controlador del otro. Los
/// que usan `poll` toman turno aquí.
static TURNSTILE: Mutex<()> = Mutex::new(());

struct Alone(#[allow(dead_code)] lock::MutexGuard<'static, ()>);

impl Alone {
    fn new() -> Self {
        let g = TURNSTILE.lock();
        set_poll_instance(None);
        Self(g)
    }
}

impl Drop for Alone {
    fn drop(&mut self) {
        set_poll_instance(None);
    }
}

fn ctrl(mut x: XhciInner) -> Arc<XhciUsbHid> {
    // Lo que hace `probe` tras enumerar: sin esto, la primera vuelta del
    // sondeo vuelve a enumerar la raíz y se lleva por delante los endpoints
    // que el test acaba de armar.
    x.boot_enum_pending = false;
    Arc::new(XhciUsbHid {
        listener: EventListener::new(),
        inner: Mutex::new(Some(x)),
        msi_vector: 0,
        halted: AtomicBool::new(false),
        has_rel_mouse_flag: AtomicBool::new(false),
        has_tablet_flag: AtomicBool::new(false),
        ack_op_base: AtomicUsize::new(0),
        ack_rt_base: AtomicUsize::new(0),
    })
}

/// Un controlador arrancado con un ratón en el puerto 1, para que el sondeo
/// tenga algo que drenar cuando se recupere.
fn with_mouse() -> (Hw, Arc<XhciUsbHid>) {
    let hw = Hw::new(SLOTS, PORTS);
    hw.set_auto_advance(1_000);
    let mut x = hw.inner(SLOTS, PORTS, 0);
    x.reset_and_run().expect("el controlador arranca");
    hw.plug(1, HS, FakeDev::mouse());
    x.setup_device(DevTopo::root(1), HS)
        .expect("el ratón enumera");
    (hw, ctrl(x))
}

// --- la recuperación suave ------------------------------------------------

/// Un halt transitorio —un HSE, que es lo que se ve en hardware de verdad—
/// se arregla volviendo a poner RS. Eso es lo que separa un parpadeo de un
/// teclado muerto hasta reiniciar.
#[test]
fn un_halt_transitorio_se_recupera_volviendo_a_poner_rs() {
    let (hw, dev) = with_mouse();
    hw.halt();
    let mut g = dev.inner.lock();
    let xi = g.as_mut().expect("el controlador");

    assert!(xi.try_soft_recover(), "no se ha recuperado");

    assert!(hw.running(), "no se ha vuelto a poner RS");
    assert_eq!(
        xi.halt_attempts, 0,
        "el presupuesto de intentos no se ha devuelto"
    );
}

/// Hay halts que llegan con RS caído: el firmware o la SMM se llevan el
/// controlador por delante, y entonces volver a poner RS es lo único que lo
/// levanta.
#[test]
fn un_halt_con_rs_caido_se_levanta_volviendo_a_ponerlo() {
    let (hw, dev) = with_mouse();
    hw.halt_clearing_rs();

    assert!(
        dev.inner
            .lock()
            .as_mut()
            .expect("el controlador")
            .try_soft_recover(),
        "no se ha recuperado"
    );

    assert!(hw.running(), "no se ha vuelto a poner RS");
}

/// Y limpia los bits pegajosos de USBSTS antes de reintentar: HSE, EINT, PCD
/// y SRE son de borrar-escribiendo-uno, así que un error que ya pasó se
/// queda puesto y el siguiente diagnóstico lo volvería a leer como nuevo.
#[test]
fn la_recuperacion_limpia_los_errores_pegajosos() {
    let (hw, dev) = with_mouse();
    // HSE y SRE puestos, como los deja un error de bus.
    hw.set_usbsts((1 << 2) | (1 << 10));
    hw.halt();

    assert!(
        dev.inner
            .lock()
            .as_mut()
            .expect("el controlador")
            .try_soft_recover(),
        "no se ha recuperado"
    );

    let sts = hw.usbsts();
    assert_eq!(sts & (1 << 2), 0, "HSE sigue puesto: {:#x}", sts);
    assert_eq!(sts & (1 << 10), 0, "SRE sigue puesto: {:#x}", sts);
}

/// Un controlador que ya no estaba parado cuenta como recuperado sin tocar
/// nada: el halt se fue entre que se leyó y que se vino a arreglarlo, y
/// volver a escribir RS sobre uno que corre no arregla nada que estuviera
/// roto.
#[test]
fn un_halt_que_ya_se_fue_cuenta_como_recuperado() {
    let (hw, dev) = with_mouse();
    // HSE puesto, pero el controlador corriendo: no hay nada que recuperar.
    hw.set_usbsts(1 << 2);
    let mut g = dev.inner.lock();
    let xi = g.as_mut().expect("el controlador");
    xi.halt_attempts = 3;

    assert!(
        xi.try_soft_recover(),
        "un controlador sano no se da por malo"
    );

    assert_eq!(xi.halt_attempts, 0, "no se ha devuelto el presupuesto");
    assert!(hw.running());
    // Y no se le ha tocado USBSTS: los bits de error que hubiera son el
    // diagnóstico de lo que pasó, y borrarlos sin haber arreglado nada se
    // lleva por delante la pista.
    drop(g);
    assert_ne!(
        hw.usbsts() & (1 << 2),
        0,
        "le ha limpiado los errores a un controlador que corre"
    );
}

/// Un aparato que no contesta a MMIO no es recuperable desde aquí: lo que se
/// lee son unos, y escribirle RS no llega a ninguna parte. Hay que decir que
/// no, no gastar un intento.
#[test]
fn un_controlador_que_no_contesta_no_se_intenta_recuperar() {
    let (hw, dev) = with_mouse();
    let antes_de_morir = hw.resets();
    hw.go_dead();
    let mut g = dev.inner.lock();
    let xi = g.as_mut().expect("el controlador");

    assert!(
        !xi.try_soft_recover(),
        "ha dicho que recuperó un aparato que no contesta"
    );
    drop(g);
    assert!(
        !hw.running(),
        "le ha escrito RS a un aparato que no está en el bus"
    );
    // Y no le ha escrito USBCMD: lo que se lee de un aparato ausente son
    // unos, y devolverle eso al registro le pide un reset del controlador
    // entero (HCRST está en el bit 1).
    assert_eq!(
        hw.resets(),
        antes_de_morir,
        "le ha mandado un reset a un aparato que no está"
    );
}

/// La espera entre intentos existe: un halt que no se va no puede comerse el
/// tick del timer reintentando a cada vuelta. El segundo intento seguido no
/// gasta presupuesto.
#[test]
fn dos_intentos_seguidos_respetan_la_espera() {
    let hw = Hw::new(SLOTS, PORTS);
    // Sin avance de reloj: así los dos intentos caen dentro de la misma
    // ventana de espera.
    let mut x = hw.inner(SLOTS, PORTS, 0);
    x.reset_and_run().expect("el controlador arranca");
    hw.never_run();
    hw.halt();

    assert!(!x.try_soft_recover(), "un controlador que no arranca");
    let tras_el_primero = x.halt_attempts;
    assert!(
        !x.try_soft_recover(),
        "el segundo intento tendría que rendirse"
    );

    assert_eq!(
        x.halt_attempts, tras_el_primero,
        "el segundo intento ha gastado presupuesto dentro de la espera"
    );
}

// --- el sondeo de respaldo ------------------------------------------------

/// El sondeo es el respaldo para MSI perdidos, y con el controlador sano
/// drena lo que haya llegado: ésa es la vuelta normal.
#[test]
fn el_sondeo_drena_los_informes_que_llegaron() {
    let _alone = Alone::new();
    let (hw, dev) = with_mouse();
    let dci = dev.inner.lock().as_ref().expect("el controlador").hids[0].ep_dci;
    hw.send_in(1, dci, &[0x00, 7, 0, 0]);
    set_poll_instance(Some(dev.clone()));

    poll();

    assert_eq!(
        dev.inner.lock().as_ref().expect("el controlador").hids[0].report_count,
        1,
        "el sondeo no ha drenado el informe"
    );
    assert!(
        !dev.halted.load(Ordering::Relaxed),
        "ha latchado un controlador sano"
    );
}

/// Un aparato que se cayó del bus se latcha de una vez: insistir cuesta una
/// ronda de MMIO en cada tick del timer, y lo típico es un xHCI de USB-C de
/// una gráfica sin nada enchufado.
#[test]
fn un_controlador_que_no_responde_se_deja_de_sondear() {
    let _alone = Alone::new();
    let (hw, dev) = with_mouse();
    hw.go_dead();
    set_poll_instance(Some(dev.clone()));

    poll();

    assert!(
        dev.halted.load(Ordering::Relaxed),
        "no se ha dejado de sondear un aparato que no contesta"
    );
    // Y una vez latchado no se vuelve a mirar: el camino rápido sale antes de
    // pedir el cerrojo.
    hw.revive();
    let dci = dev.inner.lock().as_ref().expect("el controlador").hids[0].ep_dci;
    hw.send_in(1, dci, &[0x00, 7, 0, 0]);
    poll();
    assert!(
        dev.halted.load(Ordering::Relaxed),
        "ha vuelto a sondear un controlador ya latchado"
    );
    assert_eq!(
        dev.inner.lock().as_ref().expect("el controlador").hids[0].report_count,
        0,
        "ha drenado un controlador que ya se dejó por muerto"
    );
}

/// Un halt que no se va se intenta unas cuantas veces y luego se latcha. Lo
/// que no puede hacer es latcharse en el primero, que es lo que dejaba la
/// entrada muerta en un parpadeo del bus.
#[test]
fn un_halt_que_no_se_recupera_se_latcha_pero_no_en_el_primer_intento() {
    let _alone = Alone::new();
    let (hw, dev) = with_mouse();
    // Cada lectura de registro mueve el reloj más que la espera entre
    // intentos, así que cada vuelta del sondeo gasta un intento de verdad.
    hw.set_auto_advance(600_000);
    hw.never_run();
    hw.halt();
    set_poll_instance(Some(dev.clone()));

    poll();
    assert!(
        !dev.halted.load(Ordering::Relaxed),
        "se ha rendido en el primer intento"
    );

    for _ in 0..MAX_HALT_RECOVERY_ATTEMPTS + 1 {
        poll();
    }

    assert!(
        dev.halted.load(Ordering::Relaxed),
        "no se ha rendido tras {} intentos",
        MAX_HALT_RECOVERY_ATTEMPTS
    );
}

/// Y un controlador que se recupera devuelve el presupuesto: el halt
/// siguiente, si llega, tiene sus intentos enteros otra vez. Sin esto, ocho
/// parpadeos a lo largo de horas acabarían latchando un controlador que se
/// recuperó de todos.
#[test]
fn un_controlador_que_vuelve_a_estar_sano_recupera_su_presupuesto() {
    let _alone = Alone::new();
    let (_hw, dev) = with_mouse();
    dev.inner
        .lock()
        .as_mut()
        .expect("el controlador")
        .halt_attempts = 5;
    set_poll_instance(Some(dev.clone()));

    poll();

    assert_eq!(
        dev.inner
            .lock()
            .as_ref()
            .expect("el controlador")
            .halt_attempts,
        0,
        "el presupuesto no se ha devuelto con el controlador sano"
    );
}

/// El sondeo no espera cerrojos: corre desde el tick del timer, y el camino
/// HID coge el framebuffer mientras la consola coge el suyo. Bloquear aquí
/// es el ciclo que cerró el cuelgue con `shadow_fb`.
#[test]
fn el_sondeo_no_espera_a_quien_tenga_el_cerrojo() {
    let _alone = Alone::new();
    let (hw, dev) = with_mouse();
    let dci = dev.inner.lock().as_ref().expect("el controlador").hids[0].ep_dci;
    hw.send_in(1, dci, &[0x00, 7, 0, 0]);
    set_poll_instance(Some(dev.clone()));
    let ocupado = dev.inner.lock();

    poll();

    drop(ocupado);
    assert_eq!(
        dev.inner.lock().as_ref().expect("el controlador").hids[0].report_count,
        0,
        "ha drenado sin el cerrojo, o se ha quedado esperándolo"
    );
}
