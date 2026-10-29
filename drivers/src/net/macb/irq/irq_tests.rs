//! Las cuatro ventanas de la interrupcion, contra el GEM falso de
//! [`macb_fake`].
//!
//! Lo que estos tests fijan no es que los bits esten bien --eso lo hace
//! [`super::regmap_tests`] contra el nombre de la constante de Linux-- sino
//! las tres formas en las que esta interfaz **no** se parece a un registro de
//! mascara normal: el `IER` y el `IDR` son ordenes, el `ISR` puede limpiarse
//! al leerlo, y el filtro es contra lo que el driver armo y no contra el
//! `IMR`.

use super::super::macb_fake::*;
use super::super::*;
use super::*;
use alloc::vec;
use alloc::vec::Vec;

// ---------------------------------------------------------------------------
// Armar y desarmar
// ---------------------------------------------------------------------------

/// Armar escribe la mascara en el `IER` **y nada mas**: ni se lee el `IMR`
/// antes, ni se repiten los bits que ya estaban, porque el `IER` no es un
/// registro donde se guarde lo que uno quiere sino una orden.
#[test]
fn arming_writes_the_bits_to_the_enable_register_and_reads_nothing_first() {
    let hw = Hw::new();
    let mac = hw.mac();
    let marca = hw.write_mark();

    mac.irq_enable(MACB_IRQ_WANTED);

    assert_eq!(
        hw.writes_since(marca),
        vec![IER],
        "armar tiene que ser una sola escritura, y en el IER"
    );
}

/// Y armar **solo** toca los bits que se le pasan: los demas se quedan como
/// estaban. Un driver que escribiera la mascara entera en el `IER`
/// desarmaria... nada, porque el `IER` no desarma: le dejaria armado todo lo
/// que no queria.
#[test]
fn arming_some_bits_leaves_the_rest_as_they_were() {
    let hw = Hw::new();
    let mac = hw.mac();

    mac.irq_enable(ISR_RCOMP);
    assert_eq!(hw.peek(IMR) & ISR_RCOMP, 0, "RCOMP quedo sin armar");
    assert_eq!(
        hw.peek(IMR) & ISR_TCOMP,
        ISR_TCOMP,
        "armar RCOMP armo tambien TCOMP"
    );

    mac.irq_enable(ISR_TCOMP);
    assert_eq!(
        hw.peek(IMR) & (ISR_RCOMP | ISR_TCOMP),
        0,
        "armar TCOMP desarmo RCOMP"
    );
}

/// Desarmar va por el `IDR`, que es el otro registro. Escribir el complemento
/// en el `IER` no desarma nada.
#[test]
fn disarming_goes_through_the_disable_register_not_through_the_enable_one() {
    let hw = Hw::new();
    let mac = hw.mac();
    mac.irq_enable(MACB_IRQ_WANTED);
    let marca = hw.write_mark();

    mac.irq_disable(MACB_IRQ_WANTED);

    assert_eq!(hw.writes_since(marca), vec![IDR]);
    assert_eq!(
        hw.peek(IMR) & MACB_IRQ_WANTED,
        MACB_IRQ_WANTED,
        "quedo algo armado despues de desarmarlo"
    );
}

/// Callar la tarjeta del todo: desarmar **y** reconocer, y en ese orden.
///
/// Al contrario, un flanco que llegue entre las dos escrituras queda pendiente
/// con las interrupciones todavia armadas, y la linea se levanta antes de que
/// exista el manejador.
#[test]
fn silencing_the_card_disarms_before_it_acknowledges() {
    let hw = Hw::new();
    let mac = hw.mac();
    mac.irq_enable(MACB_IRQ_WANTED);
    hw.raise_irq(ISR_RCOMP | ISR_TCOMP | ISR_HRESP);
    let marca = hw.write_mark();

    mac.irq_disable_all();

    let escrituras = hw.writes_since(marca);
    assert_eq!(
        escrituras.first(),
        Some(&IDR),
        "lo primero tiene que ser desarmar, no limpiar"
    );
    assert!(
        escrituras.contains(&ISR),
        "no se reconocio lo que estaba pendiente"
    );
    assert_eq!(hw.peek(ISR), 0, "quedo algo pendiente");
    assert_eq!(hw.peek(IMR), !0, "quedo algo armado");
}

// ---------------------------------------------------------------------------
// El estado
// ---------------------------------------------------------------------------

/// Y callar la tarjeta desarma **todo**, no solo lo que este driver armo.
///
/// El GEM puede venir del cargador de arranque con interrupciones armadas que
/// este driver no conoce: desarmar [`MACB_IRQ_WANTED`] las dejaria vivas, y la
/// linea se levanta por una causa que el manejador ni mira.
#[test]
fn silencing_the_card_disarms_what_somebody_else_armed_too() {
    let hw = Hw::new();
    let mac = hw.mac();
    // Como si el cargador de arranque hubiera armado dos que a este driver no
    // le interesan.
    mac.irq_enable(ISR_PTZ | ISR_MFD);
    assert_eq!(hw.peek(IMR) & (ISR_PTZ | ISR_MFD), 0, "no se armaron");

    mac.irq_disable_all();

    assert_eq!(hw.peek(IMR), !0, "quedo armado algo de otro");
}

/// El filtro es contra lo que el driver armo, no contra el `IMR`.
///
/// El `ISR` lleva puesto lo que ha pasado, armado o no. Devolverlo a secas le
/// atribuiria a esta interrupcion causas que nadie armo, y el manejador las
/// reconoceria y las daria por atendidas.
#[test]
fn the_status_only_reports_what_this_driver_armed() {
    let hw = Hw::new();
    let mac = hw.mac();
    mac.irq_enable(MACB_IRQ_WANTED);
    // Las tres clases de armada --- recepcion, transmision y error de
    // bus --- y dos que no se armaron nunca. Las tres tienen que llegar: un
    // filtro que solo dejara pasar las de recepcion dejaria los descriptores
    // de transmision sin recoger para siempre.
    hw.raise_irq(ISR_RCOMP | ISR_TCOMP | ISR_HRESP | ISR_MFD | ISR_PTZ);

    assert_eq!(
        mac.irq_status(),
        ISR_RCOMP | ISR_TCOMP | ISR_HRESP,
        "el estado no trae lo armado, o trae lo que no se armo"
    );
}

/// Y no se lee el `IMR` para decidirlo: su polaridad no se puede comprobar en
/// ningun sitio, asi que el driver no depende de ella. Con el `IMR` puesto a
/// lo que se quiera, el estado es el mismo.
#[test]
fn the_status_does_not_depend_on_the_mask_registers_polarity() {
    let hw = Hw::new();
    let mac = hw.mac();
    hw.raise_irq(ISR_RCOMP);

    hw.poke(IMR, 0);
    let con_cero = mac.irq_status();
    hw.raise_irq(ISR_RCOMP);
    hw.poke(IMR, !0);
    let con_unos = mac.irq_status();

    assert_eq!(con_cero, ISR_RCOMP);
    assert_eq!(con_unos, ISR_RCOMP, "el estado cambio con el IMR");
}

/// El estado se lee **una sola vez** por interrupcion.
///
/// Con `GEM_IRQCOR` puesto, leer el `ISR` es lo que lo limpia, asi que una
/// segunda lectura devuelve cero y pierde causas. Ese silicio y el otro se
/// portan distinto, que es la peor clase de fallo, asi que la unica forma
/// segura es que una lectura sea una lectura.
#[test]
fn the_status_is_read_once_because_reading_it_can_be_what_clears_it() {
    let hw = Hw::new();
    let mac = hw.mac();
    mac.irq_enable(MACB_IRQ_WANTED);
    hw.raise_irq(ISR_RCOMP);
    let marca_w = hw.write_mark();
    let marca_r = hw.read_mark();

    let _ = mac.irq_status();

    assert!(
        hw.writes_since(marca_w).is_empty(),
        "leer el estado escribio en la ventana"
    );
    assert_eq!(
        hw.reads_since(marca_r, ISR),
        1,
        "el ISR se leyo mas de una vez"
    );
}

/// Y en el silicio donde el `ISR` **se limpia al leerlo**, lo que devuelve la
/// primera lectura es lo unico que se va a saber de esta interrupcion.
///
/// Esto es lo que hace que la cuenta de arriba no sea un detalle de estilo:
/// con `GEM_IRQCOR` puesto, un driver que lea el `ISR` dos veces se come las
/// causas en la primera y decide sobre la segunda, que ya es cero. Y en el
/// silicio sin el bit funciona, que es la peor clase de fallo.
#[test]
fn on_silicon_where_the_status_clears_on_read_the_first_read_is_the_only_one() {
    let hw = Hw::new();
    hw.poke(GEM_DCFG1, hw.peek(GEM_DCFG1) | DCFG1_IRQCOR);
    let mac = hw.mac();
    mac.irq_enable(MACB_IRQ_WANTED);
    hw.raise_irq(ISR_RCOMP | ISR_TCOMP);

    assert_eq!(mac.irq_status(), ISR_RCOMP | ISR_TCOMP);
    assert_eq!(
        mac.irq_status(),
        0,
        "el ISR no se limpio al leerlo, y el falso dejo pasar el camino malo"
    );
}

// ---------------------------------------------------------------------------
// Reconocer
// ---------------------------------------------------------------------------

/// Reconocer escribe en el `ISR` lo que se va a atender, no todo.
///
/// Un bit reconocido y no atendido es un evento perdido: en el caso del
/// `RCOMP` es una trama que se queda en el anillo hasta que llegue la
/// siguiente.
#[test]
fn acknowledging_clears_only_the_bits_it_is_given() {
    let hw = Hw::new();
    let mac = hw.mac();
    hw.raise_irq(ISR_RCOMP | ISR_TCOMP);

    mac.irq_ack(ISR_RCOMP);

    assert_eq!(hw.peek(ISR) & ISR_RCOMP, 0, "no se reconocio el RCOMP");
    assert_eq!(
        hw.peek(ISR) & ISR_TCOMP,
        ISR_TCOMP,
        "se reconocio el TCOMP, que no se pidio"
    );
}

/// Reconocer nada no escribe nada. Escribir un cero en el `ISR` no haria daño
/// en este silicio, pero una escritura en la ventana de un dispositivo nunca
/// es gratis y esto corre en el camino de la interrupcion.
#[test]
fn acknowledging_nothing_does_not_touch_the_window() {
    let hw = Hw::new();
    let mac = hw.mac();
    let marca = hw.write_mark();

    mac.irq_ack(0);

    assert!(hw.writes_since(marca).is_empty());
}

/// En el silicio donde el `ISR` se limpia **al leerlo**, escribirlo no hace
/// nada y no se escribe.
///
/// No es una optimizacion: es que ese `ISR` no es un registro de escritura, y
/// lo que una escritura haga ahi no esta documentado en ningun sitio.
#[test]
fn on_silicon_where_the_status_clears_on_read_nothing_is_written_to_it() {
    let hw = Hw::new();
    hw.poke(GEM_DCFG1, hw.peek(GEM_DCFG1) | DCFG1_IRQCOR);
    let mac = hw.mac();
    assert!(!mac.isr_clear_on_write());
    hw.raise_irq(ISR_RCOMP);
    let marca = hw.write_mark();

    mac.irq_ack(ISR_RCOMP);

    assert!(
        !hw.writes_since(marca).contains(&ISR),
        "se escribio un ISR que no se limpia escribiendolo"
    );
}

// ---------------------------------------------------------------------------
// El arreglo del RXUBR
// ---------------------------------------------------------------------------

/// El arreglo baja `NCR_RE` y lo vuelve a subir, en dos escrituras.
///
/// Una sola no sirve: lo que saca al DMA de recepcion de donde se ha quedado
/// es el flanco, no el valor final, que es el mismo con el que empezo.
#[test]
fn restarting_the_receiver_takes_it_down_and_back_up() {
    let hw = Hw::new();
    let mac = hw.mac();
    hw.poke(NCR, NCR_RE | NCR_TE | NCR_MPE);
    let marca = hw.write_mark();

    mac.rx_restart();

    let escrituras: Vec<(usize, u32)> = hw
        .writes()
        .into_iter()
        .skip(marca)
        .filter(|(off, _)| *off == NCR)
        .collect();
    assert_eq!(escrituras.len(), 2, "no son dos escrituras del NCR");
    assert_eq!(
        escrituras[0].1 & NCR_RE,
        0,
        "la primera no baja la recepcion"
    );
    assert_eq!(
        escrituras[1].1 & NCR_RE,
        NCR_RE,
        "la segunda no la vuelve a subir"
    );
    // Y la de en medio tampoco se lleva nada mas por delante: apagar la
    // transmision aqui tira la trama que el MAC tuviera a medio poner en el
    // aire, y el valor final la deja encendida otra vez, asi que el unico
    // sitio donde eso se ve es esta escritura.
    assert_eq!(
        escrituras[0].1 & (NCR_TE | NCR_MPE),
        NCR_TE | NCR_MPE,
        "al bajar la recepcion se bajo algo mas"
    );
}

/// Y no se lleva por delante ningun otro bit del `NCR`.
///
/// Se lee una sola vez y las dos escrituras salen de esa lectura: la
/// transmision y el puerto de gestion tienen que quedar como estaban, porque
/// apagar el `NCR_MPE` dejaria el MDIO devolviendo basura **sin dar error**.
#[test]
fn restarting_the_receiver_leaves_the_rest_of_the_control_register_alone() {
    let hw = Hw::new();
    let mac = hw.mac();
    hw.poke(NCR, NCR_RE | NCR_TE | NCR_MPE);

    mac.rx_restart();

    assert_eq!(
        hw.peek(NCR),
        NCR_RE | NCR_TE | NCR_MPE,
        "el NCR no quedo como estaba"
    );
}

/// Y el `NCR` se lee **una sola vez**: las dos escrituras salen de esa
/// lectura.
///
/// Con dos lecturas, lo que el MAC tenga en medio --- un bit que el propio
/// hardware baja, como el `TSTART` --- se cuela en la segunda escritura, y el
/// arreglo deja un `NCR` que nadie pidio.
#[test]
fn restarting_the_receiver_reads_the_control_register_once() {
    let hw = Hw::new();
    let mac = hw.mac();
    hw.poke(NCR, NCR_RE | NCR_TE | NCR_MPE);
    let marca = hw.read_mark();

    mac.rx_restart();

    assert_eq!(hw.reads_since(marca, NCR), 1, "el NCR se leyo dos veces");
}

/// El arreglo **deja la recepcion encendida**, tambien si estaba apagada.
///
/// Es la forma de `macb_main.c`, y no es un descuido suyo: alli solo se llama
/// desde el camino del `RXUBR`, y un `RXUBR` solo lo levanta un MAC que estaba
/// recibiendo. Se fija aqui porque **es una trampa para quien lo llame desde
/// otro sitio**: esto no es «reiniciar la recepcion si estaba en marcha», es
/// encenderla, y encenderla sin huecos en el anillo es pedirle al MAC que
/// desborde su FIFO.
#[test]
fn restarting_the_receiver_leaves_it_on_which_is_why_only_the_rxubr_path_may_call_it() {
    let hw = Hw::new();
    let mac = hw.mac();
    hw.poke(NCR, NCR_MPE);

    mac.rx_restart();

    assert_eq!(
        hw.peek(NCR),
        NCR_MPE | NCR_RE,
        "el arreglo no dejo la recepcion encendida"
    );
}

// ---------------------------------------------------------------------------
// Los grupos
// ---------------------------------------------------------------------------

/// Lo que se arma lleva dentro las dos de completado y el `HRESP`, y **no**
/// lleva el `RXUBR` ni el `MFD`.
///
/// El `RXUBR` lo arma Linux solo en dos versiones de silicio con una errata, y
/// el GEM del RP1 no es ninguna de las dos: armarlo aqui seria una
/// interrupcion por cada vez que la recepcion se queda corta. El `MFD` es del
/// MDIO, que se usa con un plazo.
#[test]
fn what_gets_armed_is_the_two_completions_and_the_bus_error() {
    assert_eq!(MACB_IRQ_WANTED & ISR_RCOMP, ISR_RCOMP);
    assert_eq!(MACB_IRQ_WANTED & ISR_TCOMP, ISR_TCOMP);
    assert_eq!(MACB_IRQ_WANTED & ISR_HRESP, ISR_HRESP);
    assert_eq!(MACB_IRQ_WANTED & ISR_RXUBR, 0, "se armo el RXUBR");
    assert_eq!(MACB_IRQ_WANTED & ISR_MFD, 0, "se armo el MFD");
    assert_eq!(MACB_IRQ_WANTED & ISR_LINK, 0, "se armo el LINK");
}

/// Los tres errores de transmision van **dentro** del grupo de transmision, no
/// aparte: los tres acaban en lo mismo, que es que hay descriptores que
/// recoger y una cuenta que subir.
#[test]
fn the_transmit_errors_are_part_of_the_transmit_group() {
    assert_eq!(MACB_TX_INT_FLAGS & MACB_TX_ERR_FLAGS, MACB_TX_ERR_FLAGS);
    assert_eq!(MACB_TX_ERR_FLAGS, ISR_TUND | ISR_RLE | ISR_TXERR);
}

/// El `TXUBR` esta en el grupo de transmision aunque **no sea un error**: es
/// el centinela que la tanda 2 planta detras de cada trama, y el MAC avisa de
/// haberlo leido. Sin el, una tanda de transmisiones que acabe justo en el
/// centinela no despierta a nadie y los descriptores se quedan sin recoger.
#[test]
fn the_sentinel_being_read_is_a_transmit_wakeup_and_not_an_error() {
    assert_eq!(MACB_TX_INT_FLAGS & ISR_TXUBR, ISR_TXUBR);
    assert_eq!(
        MACB_TX_ERR_FLAGS & ISR_TXUBR,
        0,
        "el TXUBR cuenta como error"
    );
}

/// Y los dos grupos no comparten ningun bit: el manejador decide que hacer por
/// grupos, y un bit en los dos haria que recoger descriptores de transmision
/// contase como trama recibida.
#[test]
fn the_two_groups_do_not_share_a_bit() {
    assert_eq!(MACB_RX_INT_FLAGS & MACB_TX_INT_FLAGS, 0);
    assert_eq!(MACB_RX_INT_FLAGS, ISR_RCOMP | ISR_ROVR);
}

/// Y todo lo que se arma cae dentro de los bits que el registro define: un bit
/// armado fuera de ellos es una escritura en un bit reservado.
#[test]
fn nothing_armed_falls_outside_the_bits_the_register_defines() {
    let definidos = ISR_MFD
        | ISR_RCOMP
        | ISR_RXUBR
        | ISR_TXUBR
        | ISR_TUND
        | ISR_RLE
        | ISR_TXERR
        | ISR_TCOMP
        | ISR_LINK
        | ISR_ROVR
        | ISR_HRESP
        | ISR_PFR
        | ISR_PTZ;
    assert_eq!(MACB_IRQ_WANTED & !definidos, 0);
}
