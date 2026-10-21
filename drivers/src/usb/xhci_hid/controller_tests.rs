//! El arranque del controlador y el camino de los comandos.
//!
//! Hasta aquí ningún test de este módulo llamaba a `XhciInner`: todos eran de
//! funciones puras (clasificar un descriptor, decodificar un informe, construir
//! un TRB), porque el driver habla por MMIO y no había nada al otro lado.
//! [`super::xhci_fake`] pone ese otro lado, así que `reset_and_run` y el camino
//! de comandos se pueden ejecutar de verdad: lo que estos tests comprueban es
//! lo que el controlador ve, registro a registro y TRB a TRB.

use super::xhci_fake::Hw;
use super::*;
use crate::nvme::nvme_queue::test_clock;
use crate::utils::host_hooks as shim;
use core::sync::atomic::Ordering;

/// Offsets que los tests nombran (dentro de la ventana operacional).
const USBCMD: usize = 0x00;
const CRCR: usize = 0x18;
const DCBAAP: usize = 0x30;
const CONFIG: usize = 0x38;

const SLOTS: u8 = 8;
const PORTS: u8 = 4;

fn hw() -> Hw {
    Hw::new(SLOTS, PORTS)
}

/// El arranque entero, que es una secuencia de ocho pasos de los que el
/// controlador se queda con seis valores: si uno se cae, el controlador
/// arranca sin saber dónde están sus anillos.
#[test]
fn el_arranque_le_deja_al_controlador_todo_lo_que_necesita() {
    let hw = hw();
    let mut x = hw.inner(SLOTS, PORTS, 0);
    let t0 = clock_now();
    x.reset_and_run().expect("el controlador arranca");
    let coste = clock_now() - t0;

    // Lo único que el arranque de un controlador que contesta tiene que
    // costar son los 100 ms de VBUS estable que exige USB tras dar energía.
    // Cada plazo que se espera de más --el de HCHalted son otros 100 ms-- se
    // los lleva el arranque de la máquina entera, una vez por controlador.
    assert!(
        coste < 150_000,
        "el arranque ha costado {} us: se está esperando un plazo que ya se \
         cumplió",
        coste
    );
    assert_eq!(hw.resets(), 1, "el controlador no se ha reseteado una vez");
    assert!(hw.running(), "el controlador no se ha quedado corriendo");
    assert_eq!(
        hw.config_slots(),
        SLOTS,
        "CONFIG no lleva los slots que el driver dice soportar"
    );
    assert_eq!(
        hw.op(CONFIG) & 0xff,
        SLOTS as u32,
        "CONFIG no se ha quedado escrito en la BAR"
    );
    assert_eq!(
        hw.op64(DCBAAP),
        x.dcbaa.phys as u64,
        "DCBAAP no se ha quedado escrito en la BAR"
    );
    assert_eq!(
        hw.op64(CRCR),
        x.cmd.crcr() | 1,
        "CRCR no se ha quedado escrito en la BAR"
    );
    assert_eq!(
        hw.dcbaap(),
        x.dcbaa.phys as u64,
        "DCBAAP no apunta al array de contextos de dispositivo"
    );
    assert_eq!(
        hw.crcr() & !0x3f,
        x.cmd.crcr(),
        "CRCR no apunta al anillo de comandos"
    );
    assert_eq!(
        hw.crcr() & 1,
        1,
        "CRCR no lleva el bit de ciclo del productor"
    );
    assert_eq!(hw.erstsz(), 1, "ERSTSZ no dice un segmento");
    assert_eq!(
        hw.erstba(),
        x.ev.erst_phys(),
        "ERSTBA no apunta a la tabla del anillo de eventos"
    );
    assert_eq!(
        hw.erdp_writes().first().copied(),
        Some(x.ev.seg.phys as u64 | 8),
        "ERDP no arranca en el primer TRB del segmento con EHB puesto"
    );
    assert_eq!(hw.op(USBCMD) & 1, 1, "RS no se ha quedado puesto");
    assert_eq!(
        hw.op(USBCMD) & (1 << 2),
        0,
        "INTE puesto sin MSI: el controlador interrumpe a nadie"
    );
    assert_eq!(
        hw.iman(),
        1,
        "IMAN sin MSI no se ha quedado en IP limpio, IE=0"
    );
}

/// Con MSI el interrupter se habilita, y es el único cambio: la misma
/// secuencia con un vector distinto de cero.
#[test]
fn con_msi_el_interrupter_y_el_controlador_se_habilitan() {
    let hw = hw();
    let mut x = hw.inner(SLOTS, PORTS, 7);
    x.reset_and_run().expect("el controlador arranca");
    assert_eq!(hw.iman(), 3, "IMAN no lleva IE=1 con MSI");
    assert_eq!(
        hw.op(USBCMD) & (1 << 2),
        1 << 2,
        "INTE no se ha puesto con MSI"
    );
}

/// Un controlador que ya corre (lo dejó la BIOS) hay que pararlo antes de
/// resetearlo, y *esperar* a que se pare. Lo que se mide es la espera: el
/// reloj de test avanza una vez por lectura de registro, así que un plazo
/// agotado se ve como 100 ms de reloj que no pasan cuando el controlador
/// contesta.
#[test]
fn a_un_controlador_que_ya_corria_se_le_espera_a_que_pare() {
    let hw = hw();
    hw.already_running();
    let mut x = hw.inner(SLOTS, PORTS, 0);
    let t0 = clock_now();
    x.reset_and_run().expect("el controlador arranca");
    let parando = clock_now() - t0;

    let hw2 = hw2_never_halting();
    let mut y = hw2.inner(SLOTS, PORTS, 0);
    let t1 = clock_now();
    y.reset_and_run().expect("el controlador arranca igual");
    let esperando = clock_now() - t1;

    // El plazo son 100 ms y la espera sale en cuanto los ha cumplido, así que
    // la diferencia es de 100 ms menos la última vuelta.
    assert!(
        esperando - parando >= 90_000,
        "un controlador que no se para no ha costado el plazo de 100 ms: \
         {} us contra {} us",
        esperando,
        parando
    );
}

/// CNR puesto significa que el controlador no está listo para nada, y lo que
/// no se puede hacer es resetearlo de todas formas: eso es un reset emitido
/// contra un registro que el controlador no está leyendo.
#[test]
fn con_cnr_puesto_no_se_emite_el_reset() {
    let hw = hw();
    hw.set_read_tick(20_000); // el plazo de CNR es de 1 s
    hw.hold_cnr_for(u32::MAX);
    let mut x = hw.inner(SLOTS, PORTS, 0);
    assert_eq!(
        x.reset_and_run(),
        Err(DeviceError::NotReady),
        "un controlador que no está listo ha arrancado"
    );
    assert_eq!(hw.resets(), 0, "se ha emitido HCRST con CNR puesto");
    assert_eq!(hw.op(USBCMD) & 2, 0, "HCRST se ha quedado escrito");
}

/// CNR que se limpia dentro del plazo: el arranque sigue adelante.
#[test]
fn un_cnr_que_se_limpia_a_tiempo_no_para_el_arranque() {
    let hw = hw();
    hw.hold_cnr_for(3);
    let mut x = hw.inner(SLOTS, PORTS, 0);
    x.reset_and_run().expect("el controlador arranca");
    assert_eq!(
        hw.resets(),
        1,
        "el reset no se ha emitido tras limpiarse CNR"
    );
}

/// Un controlador que acepta RS y se queda en HCHalted no está corriendo, y
/// decir que sí lo está es lo que deja al driver esperando eventos de un
/// controlador parado.
#[test]
fn un_controlador_que_no_sale_de_halted_no_arranca() {
    let hw = hw();
    hw.set_read_tick(2_000);
    hw.never_run();
    let mut x = hw.inner(SLOTS, PORTS, 0);
    assert_eq!(
        x.reset_and_run(),
        Err(DeviceError::NotReady),
        "un controlador parado ha arrancado"
    );
    assert_eq!(hw.op(USBCMD) & 1, 1, "RS no se ha intentado siquiera");
}

/// Un controlador que no sale del reset tampoco arranca: HCRST se queda
/// puesto, así que RS no lo toma nadie.
#[test]
fn un_controlador_que_no_sale_del_reset_no_arranca() {
    let hw = hw();
    hw.set_read_tick(2_000);
    hw.stick_in_reset();
    let mut x = hw.inner(SLOTS, PORTS, 0);
    assert_eq!(
        x.reset_and_run(),
        Err(DeviceError::NotReady),
        "un controlador que sigue en reset ha arrancado"
    );
    assert_eq!(hw.op(USBCMD) & 2, 2, "HCRST no se ha quedado puesto");
    assert!(!hw.running(), "el controlador corre en medio de un reset");
}

/// Los scratchpad buffers son memoria que el controlador pide para él, y los
/// pide en dos campos de HCSPARAMS2: los cinco bits bajos y los cinco altos.
/// Leer solo los bajos le da 1 página a un controlador que pidió 33.
#[test]
fn los_scratchpad_buffers_salen_de_los_dos_campos_de_hcsparams2() {
    for want in [1u32, 5, 31, 33] {
        let hw = hw();
        let lo = want & 0x1f;
        let hi = want >> 5;
        hw.set_cap(0x08, (lo << 27) | (hi << 21));
        let mut x = hw.inner(SLOTS, PORTS, 0);
        x.reset_and_run().expect("el controlador arranca");

        assert_eq!(
            x.scratch_pages.len(),
            want as usize,
            "se han reservado {} páginas de scratchpad en vez de {}",
            x.scratch_pages.len(),
            want
        );
        let tbl = x.dcbaa.read_u64(0);
        assert_ne!(tbl, 0, "la entrada 0 del DCBAA no apunta a la tabla");
        assert_eq!(
            tbl,
            x.scratch_tbl.as_ref().expect("la tabla").phys as u64,
            "la entrada 0 del DCBAA no es la tabla de scratchpad"
        );
        for i in 0..want as usize {
            let page = unsafe { read_volatile((tbl as usize + i * 8) as *const u64) };
            assert_eq!(
                page, x.scratch_pages[i].phys as u64,
                "la entrada {} de la tabla no es su página",
                i
            );
        }
    }
}

/// Sin scratchpad la entrada 0 del DCBAA es del controlador y tiene que
/// quedarse a cero: ahí escribe el driver la tabla, y escribirla sin que haga
/// falta le da al controlador una página que nadie reservó.
#[test]
fn sin_scratchpad_la_entrada_cero_del_dcbaa_se_queda_a_cero() {
    let hw = hw();
    hw.set_cap(0x08, 0);
    let mut x = hw.inner(SLOTS, PORTS, 0);
    x.reset_and_run().expect("el controlador arranca");
    assert_eq!(
        x.dcbaa.read_u64(0),
        0,
        "la entrada 0 del DCBAA no está limpia"
    );
    assert!(
        x.scratch_tbl.is_none(),
        "hay tabla de scratchpad sin pedirla"
    );
    assert!(x.scratch_pages.is_empty(), "hay páginas sin pedirlas");
}

/// Si la memoria se acaba a mitad de los scratchpad, el controlador no puede
/// arrancar: arrancarlo igual lo deja leyendo la tabla a medio llenar.
#[test]
fn si_no_hay_memoria_para_un_scratchpad_el_controlador_no_arranca() {
    let hw = hw();
    hw.set_cap(0x08, 4 << 27); // cuatro páginas
    let mut x = hw.inner(SLOTS, PORTS, 0);
    // El driver ya tiene sus anillos; se corta la tercera página de las
    // cuatro, que es la única forma de llegar al camino que devuelve las dos
    // primeras.
    shim::ALLOC_CALLS.store(0, Ordering::SeqCst);
    shim::FAIL_ALLOC_AFTER.store(3, Ordering::SeqCst);
    let out = x.reset_and_run();
    shim::FAIL_ALLOC_AFTER.store(usize::MAX, Ordering::SeqCst);
    assert_eq!(
        out,
        Err(DeviceError::DmaError),
        "el arranque ha seguido sin memoria para un scratchpad"
    );
    assert!(
        !hw.running(),
        "el controlador se ha puesto en marcha con la tabla a medias"
    );
}

/// La energía de los puertos se da con una reescritura de PORTSC, y una
/// reescritura de PORTSC puede borrar lo que no ha leído nadie: CSC es
/// write-1-to-clear, así que encender el puerto de un dispositivo que acaba de
/// conectarse no puede perder su cambio de conexión.
#[test]
fn dar_energia_a_un_puerto_no_borra_su_cambio_de_conexion() {
    let hw = hw();
    hw.connect(1, SPEED_HIGH);
    assert_eq!(
        hw.portsc(1) & (1 << 17),
        1 << 17,
        "el fake no ha puesto CSC"
    );
    // El puerto 2 ya tiene energía: no se toca.
    hw.set_portsc(2, 1 << 9);
    let mut x = hw.inner(SLOTS, PORTS, 0);
    x.reset_and_run().expect("el controlador arranca");

    let sc = hw.portsc(1);
    assert_eq!(
        sc & (1 << 9),
        1 << 9,
        "el puerto 1 se ha quedado sin energía"
    );
    assert_eq!(
        sc & (1 << 17),
        1 << 17,
        "encender el puerto 1 ha borrado su CSC"
    );
    assert_eq!(
        sc & 1,
        1,
        "el puerto 1 ha perdido la presencia del dispositivo"
    );
    assert_eq!(
        hw.portsc(2),
        1 << 9,
        "el puerto 2, que ya tenía energía, se ha reescrito"
    );
}

/// Un comando es un TRB en el anillo, un timbre en el doorbell 0 y un evento
/// de terminación de vuelta. El driver espera por la *dirección* del TRB, que
/// es lo único que distingue un evento de este comando del de otro.
#[test]
fn un_comando_se_timbra_y_se_espera_por_su_direccion() {
    let hw = hw();
    let mut x = hw.inner(SLOTS, PORTS, 0);
    x.reset_and_run().expect("el controlador arranca");
    let timbres = hw.doorbells().len();
    let erdps = hw.erdp_writes().len();

    x.exec_cmd(trb_disable_slot(3)).expect("el comando termina");

    assert_eq!(
        hw.doorbells()[timbres..],
        [(0, 0)],
        "el comando no ha timbrado el doorbell del controlador"
    );
    let cmds = hw.commands();
    assert_eq!(cmds.len(), 1, "el controlador no ha consumido un comando");
    assert_eq!(cmds[0].ty, 10, "el TRB consumido no es un Disable Slot");
    assert_eq!(cmds[0].slot, 3, "el comando no iba al slot 3");
    assert_eq!(
        hw.erdp_writes().len(),
        erdps + 1,
        "el evento consumido no ha movido ERDP"
    );
}

/// Un comando de endpoint lleva su DCI en los bits 20:16, que es otro campo
/// del mismo dword: meterlo donde va el slot para un endpoint del dispositivo
/// equivocado.
#[test]
fn un_comando_de_endpoint_lleva_su_slot_y_su_dci() {
    let hw = hw();
    let mut x = hw.inner(SLOTS, PORTS, 0);
    x.reset_and_run().expect("el controlador arranca");
    x.exec_cmd(trb_stop_endpoint(4, 3))
        .expect("el comando termina");
    let cmds = hw.commands();
    assert_eq!(cmds[0].ty, 15, "el TRB consumido no es un Stop Endpoint");
    assert_eq!(cmds[0].slot, 4, "el comando no iba al slot 4");
    assert_eq!(cmds[0].dci, 3, "el comando no iba al endpoint 3");
    assert_eq!(cmds[0].param, 0, "un Stop Endpoint no lleva parámetro");
}

/// Un comando que el controlador rechaza es un error, y no un silencio: el
/// código de terminación es lo único que lo dice.
#[test]
fn un_comando_que_el_controlador_rechaza_da_error() {
    let hw = hw();
    hw.answer_commands_with(5); // TRB Error
    let mut x = hw.inner(SLOTS, PORTS, 0);
    x.reset_and_run().expect("el controlador arranca");
    assert_eq!(
        x.exec_cmd(trb_disable_slot(3)),
        Err(DeviceError::IoError),
        "un comando rechazado ha salido bien"
    );
}

/// Un Short Packet no es un fallo del comando, que es la otra mitad de la
/// condición que decide.
#[test]
fn un_comando_terminado_con_short_packet_sale_bien() {
    let hw = hw();
    hw.answer_commands_with(TRB_CC_SHORT);
    let mut x = hw.inner(SLOTS, PORTS, 0);
    x.reset_and_run().expect("el controlador arranca");
    x.exec_cmd(trb_disable_slot(3))
        .expect("un short packet no es un comando fallido");
}

/// Un controlador que no contesta deja al driver en su plazo de 5 s, y el
/// plazo tiene que terminar: sin él, el arranque se queda ahí para siempre.
#[test]
fn un_comando_sin_respuesta_termina_en_su_plazo() {
    let hw = hw();
    let mut x = hw.inner(SLOTS, PORTS, 0);
    x.reset_and_run().expect("el controlador arranca");
    hw.go_deaf();
    hw.set_read_tick(100_000);
    let t0 = clock_now();
    assert_eq!(
        x.exec_cmd(trb_disable_slot(3)),
        Err(DeviceError::IoError),
        "un comando sin respuesta ha salido bien"
    );
    assert!(
        clock_now() - t0 >= 5_000_000,
        "la espera ha salido antes de su plazo de 5 s"
    );
}

/// Enable Slot es el único comando cuyo resultado *es* un dato: el slot que
/// el controlador reparte, en los bits 31:24 del dword de control del evento.
/// Leerlo del sitio equivocado da el slot 0, que no es ningún slot.
#[test]
fn enable_slot_devuelve_el_slot_que_reparte_el_controlador() {
    let hw = hw();
    let mut x = hw.inner(SLOTS, PORTS, 0);
    x.reset_and_run().expect("el controlador arranca");

    for want in 1..=3u8 {
        let p = x
            .cmd
            .push(trb_enable_slot())
            .expect("el anillo tiene sitio");
        x.mmio.ring_db(0, 0);
        assert_eq!(
            x.wait_cmd_phys_slot(p),
            Ok(want),
            "el slot repartido no es el {}",
            want
        );
    }
}

/// Un Short Packet tampoco es un fallo en un comando de slot, que es la otra
/// mitad de *esa* condición: es la del slot, no la de `wait_cmd_phys`.
#[test]
fn un_enable_slot_terminado_con_short_packet_reparte_su_slot() {
    let hw = hw();
    hw.answer_commands_with(TRB_CC_SHORT);
    let mut x = hw.inner(SLOTS, PORTS, 0);
    x.reset_and_run().expect("el controlador arranca");
    let p = x.cmd.push(trb_enable_slot()).expect("sitio");
    x.mmio.ring_db(0, 0);
    assert_eq!(
        x.wait_cmd_phys_slot(p),
        Ok(1),
        "un short packet ha tirado el slot que el controlador repartió"
    );
}

/// Un Enable Slot que el controlador rechaza no reparte slot, y devolver uno
/// de todas formas es direccionar un slot que no existe.
#[test]
fn un_enable_slot_rechazado_no_devuelve_slot() {
    let hw = hw();
    hw.answer_commands_with(9); // No Slots Available Error
    let mut x = hw.inner(SLOTS, PORTS, 0);
    x.reset_and_run().expect("el controlador arranca");
    let p = x
        .cmd
        .push(trb_enable_slot())
        .expect("el anillo tiene sitio");
    x.mmio.ring_db(0, 0);
    assert_eq!(
        x.wait_cmd_phys_slot(p),
        Err(DeviceError::IoError),
        "un Enable Slot rechazado ha repartido un slot"
    );
}

/// La espera de un Enable Slot tiene su propio plazo, y es el mismo: un
/// controlador que no contesta no puede dejar la enumeración colgada.
#[test]
fn un_enable_slot_sin_respuesta_termina_en_su_plazo() {
    let hw = hw();
    let mut x = hw.inner(SLOTS, PORTS, 0);
    x.reset_and_run().expect("el controlador arranca");
    hw.go_deaf();
    hw.set_read_tick(100_000);
    let t0 = clock_now();
    let p = x.cmd.push(trb_enable_slot()).expect("sitio");
    x.mmio.ring_db(0, 0);
    assert_eq!(
        x.wait_cmd_phys_slot(p),
        Err(DeviceError::IoError),
        "un Enable Slot sin respuesta ha repartido un slot"
    );
    assert!(
        clock_now() - t0 >= 5_000_000,
        "la espera del slot ha salido antes de su plazo de 5 s"
    );
}

/// El evento de *otro* comando no cierra esta espera. Dos comandos seguidos
/// contestados en orden: el segundo tiene que esperar a su propio evento, y
/// comparar mal las direcciones hace que cada espera se quede con el evento
/// que encuentra.
#[test]
fn el_evento_de_otro_comando_no_cierra_esta_espera() {
    let hw = hw();
    let mut x = hw.inner(SLOTS, PORTS, 0);
    x.reset_and_run().expect("el controlador arranca");

    let primero = x.cmd.push(trb_disable_slot(1)).expect("sitio");
    let segundo = x.cmd.push(trb_disable_slot(2)).expect("sitio");
    x.mmio.ring_db(0, 0);
    assert_eq!(
        hw.events_posted(),
        2,
        "el controlador no ha contestado los dos"
    );

    // La espera del segundo se encuentra primero el evento del primero, y
    // tiene que seguir esperando.
    x.wait_cmd_phys(segundo)
        .expect("el segundo comando termina");
    // Y el del primero ya se consumió, así que su espera se va al plazo.
    hw.set_read_tick(100_000);
    assert_eq!(
        x.wait_cmd_phys(primero),
        Err(DeviceError::IoError),
        "la espera del primero ha cerrado con un evento que ya no está"
    );
}

/// Un evento de cambio de puerto se *difiere*: atenderlo dentro de `pop_ev`
/// es enumerar un puerto desde dentro del drenaje del anillo de eventos, que
/// es como se monta la re-entrada que ya paró esta máquina.
#[test]
fn un_evento_de_puerto_se_difiere_en_vez_de_atenderse() {
    let hw = hw();
    let mut x = hw.inner(SLOTS, PORTS, 0);
    x.reset_and_run().expect("el controlador arranca");

    for port in [2u8, 2, 0] {
        post_port_event(&x, port);
        assert!(
            x.pop_ev(None).is_some(),
            "el evento de puerto no se ha leído"
        );
    }
    assert_eq!(
        x.pending_port_changes,
        alloc::vec![2],
        "los cambios de puerto diferidos no son el puerto 2 una sola vez"
    );
}

/// El anillo de eventos da la vuelta, y al darla el consumidor invierte su
/// bit de ciclo: si no lo invierte, el primer TRB de la segunda vuelta --que
/// lleva el ciclo nuevo-- parece vacío y el anillo se queda muerto.
#[test]
fn el_anillo_de_eventos_da_la_vuelta_sin_morirse() {
    let hw = hw();
    let mut x = hw.inner(SLOTS, PORTS, 0);
    x.reset_and_run().expect("el controlador arranca");
    let vueltas = x.ev.ntrb;

    // Una vuelta entera y un comando más, cada uno esperado por su dirección.
    for _ in 0..=vueltas {
        let p = x.cmd.push(trb_disable_slot(1)).expect("sitio");
        x.mmio.ring_db(0, 0);
        x.wait_cmd_phys(p)
            .expect("el comando termina tras la vuelta");
    }
    assert!(
        !x.ev.cycle,
        "el consumidor no ha invertido su ciclo al dar la vuelta"
    );
}

// ─── Utilidades ─────────────────────────────────────────────────────────────

fn clock_now() -> u64 {
    // El reloj avanza en cada lectura, así que leerlo aquí también cuenta: lo
    // que se compara son diferencias de centenas de milisegundos.
    test_clock::now()
}

/// Un segundo controlador, para el test que compara dos arranques. El fake es
/// uno por hilo, así que el primero tiene que haberse ido ya.
fn hw2_never_halting() -> Hw {
    let hw = Hw::new(SLOTS, PORTS);
    hw.already_running();
    hw.never_halt();
    hw
}

/// Mete un evento de cambio de puerto en el anillo de eventos del driver, que
/// es lo que el controlador hace cuando algo se conecta.
fn post_port_event(x: &XhciInner, port: u8) {
    let off = x.ev.deq * 16;
    x.ev.seg.write_u64(off, (port as u64) << 24);
    x.ev.seg.write_u32(off + 8, TRB_CC_SUCCESS << 24);
    x.ev.seg
        .write_u32(off + 12, TRB_EVT_PORT_STATUS | x.ev.cycle as u32);
}
