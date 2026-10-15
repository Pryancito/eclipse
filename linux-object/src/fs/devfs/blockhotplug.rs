//! Nodos de `/dev` para los discos que llegan y se van después del arranque.
//!
//! `create_root_fs` recorre `all_block()` **una vez** y crea un nodo por disco
//! y otro por partición. Un disco que apareciera más tarde --un pendrive, un
//! disco externo-- quedaba dado de alta en `kernel-hal` y sin nada que abrir:
//! `/sys/block` sí lo listaba, porque recalcula su lista en cada lectura, pero
//! `/dev` no, y lo que monta la gente es `/dev`. Eso es «el pendrive se ve pero
//! el hotplug no funciona».
//!
//! El reparto de trabajo entre las dos mitades es lo único delicado:
//!
//! - [`observe`] es lo que llama `kernel-hal` en cuanto se da de alta o de baja
//!   un disco, y corre **en el contexto de quien lo descubrió**: para USB, un
//!   tick del timer, o sea una interrupción. Ahí no se puede hacer E/S ni coger
//!   un cerrojo que alguien pueda tener (el del controlador se coge desde un
//!   manejador de interrupción), así que sólo apunta el disco en una cola y
//!   despierta a la tarea.
//! - [`BlockHotplugFuture`] es una tarea del kernel, y es la que crea los
//!   nodos. Crear el nodo del disco entero no cuesta E/S, pero buscarle las
//!   particiones sí: hay que leer el MBR y la cabecera GPT, y en un pendrive
//!   lento eso son milisegundos con el cerrojo del controlador cogido. Por eso
//!   vive en una tarea y no en el aviso.

use alloc::{
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

use kernel_hal::sync::Mutex;
use lazy_static::lazy_static;
use zcore_drivers::scheme::BlockScheme;

use super::blockdev::{disk_node_name, partition_node_name, BlockDev};

lazy_static! {
    /// Discos por atender: `true` si llegó, `false` si se fue.
    ///
    /// Se conserva el orden, porque enchufar y desenchufar el mismo puerto dos
    /// veces seguidas tiene que acabar en el estado del último aviso.
    ///
    /// Nace con sitio para [`PENDING_CAP`] y no crece: quien apunta aquí corre
    /// en contexto de interrupción, y un `push` que tuviera que pedir memoria
    /// se encontraría con el cerrojo del asignador, que puede tener cogido la
    /// tarea a la que acabamos de interrumpir en esta misma CPU.
    static ref PENDING: Mutex<Vec<(Arc<dyn BlockScheme>, bool)>> =
        Mutex::new(Vec::with_capacity(PENDING_CAP));
    /// Por cada disco en caliente, el nombre que se le dio y su índice base.
    ///
    /// La identidad es el puntero del `Arc`, no el nombre del driver: dos
    /// pendrives iguales lo repiten, y el nodo que hay que borrar es el del que
    /// se fue.
    static ref CREATED: Mutex<Vec<HotDisk>> = Mutex::new(Vec::new());
    /// Una ranura por disco: su nombre mientras esté, `None` cuando se fue.
    ///
    /// Hacen falta ranuras, y no una lista de nombres, por dos razones. El
    /// nombre sale de la posición en `all_block()`, y al desenchufar un disco
    /// de en medio esa posición se corre: volver a derivarlo renombraría nodos
    /// que alguien tiene montados. Y la posición **es** el número de minor
    /// (`ranura * 16`, dejando hueco para 15 particiones), así que una lista
    /// que se encogiera al desenchufar le daría al siguiente disco el minor de
    /// uno que sigue puesto. Un hueco se reutiliza; el resto no se mueve.
    static ref SLOTS: Mutex<Vec<Option<String>>> = Mutex::new(Vec::new());
    static ref WAKER: Mutex<Option<Waker>> = Mutex::new(None);
}

/// Avisos que caben sin pedir memoria. Son muchos más de los que puede haber
/// a la vez: la tarea vacía la cola en cuanto la despiertan, y nadie enchufa
/// treinta y dos discos entre dos quantos.
const PENDING_CAP: usize = 32;

/// Un disco en caliente del que hemos creado nodos: el disco, el nombre que
/// se le dio y su índice base de minor.
type HotDisk = (Arc<dyn BlockScheme>, String, usize);

/// Cuántas particiones se barren al dar de baja un disco. Es el mismo tope
/// que usa `rescan_partitions` al crearlas, y el de Linux para un disco MBR.
const MAX_PARTS: usize = 15;

/// Coge la primera ranura libre para `name`, o la que ya tenía. Devuelve su
/// número, que es de donde sale el minor.
///
/// Función aparte y sobre un vector cualquiera para que se pueda probar: la de
/// verdad vive en un `static` que un test no puede tocar sin estorbar a los
/// demás.
fn claim_slot(slots: &mut Vec<Option<String>>, name: &str) -> usize {
    if let Some(i) = slots.iter().position(|s| s.as_deref() == Some(name)) {
        return i;
    }
    match slots.iter().position(|s| s.is_none()) {
        Some(i) => {
            slots[i] = Some(name.to_string());
            i
        }
        None => {
            slots.push(Some(name.to_string()));
            slots.len() - 1
        }
    }
}

/// Deja libre la ranura de `name`, sin mover las demás.
fn release_slot(slots: &mut [Option<String>], name: &str) {
    for s in slots.iter_mut() {
        if s.as_deref() == Some(name) {
            *s = None;
        }
    }
}

/// Los nombres ocupados ahora mismo.
fn occupied(slots: &[Option<String>]) -> Vec<&str> {
    slots.iter().filter_map(|s| s.as_deref()).collect()
}

/// Apunta el nombre que creó el recorrido del arranque, para no dárselo luego
/// a un disco en caliente.
pub fn note_boot_disk(name: &str) {
    claim_slot(&mut SLOTS.lock(), name);
}

/// El nombre libre que le toca a un disco, por la convención de Linux.
///
/// Se le pasan los nombres ya ocupados y el del driver, y cuenta hacia adelante
/// hasta encontrar uno que no esté: `sda` existiendo, un pendrive es `sdb`.
/// Derivarlo de la posición en `all_block()` sin mirar lo ocupado daría `sda`
/// otra vez en cuanto se hubiera desenchufado el disco de delante.
fn next_free_name(taken: &[&str], own_name: &str) -> String {
    // `disk_node_name` cuenta por familia, así que el nombre que pide es el que
    // sale con `n` discos de su familia delante. Se prueba `n` creciente.
    let family: &str = if own_name.starts_with("nvme") {
        "nvme"
    } else if own_name.starts_with("virtio") {
        "virtio"
    } else {
        "otro"
    };
    let mut before: Vec<&str> = Vec::new();
    for _ in 0..=taken.len() {
        let cand = disk_node_name(&before, own_name);
        if !taken.contains(&cand.as_str()) {
            return cand;
        }
        before.push(family);
    }
    // Inalcanzable: con `taken.len()+1` intentos hay al menos un nombre libre.
    disk_node_name(&before, own_name)
}

/// Lo que `kernel-hal` llama al dar de alta o de baja un disco.
///
/// Corre en contexto de interrupción: sólo apunta y despierta. Ver la cabecera
/// del módulo.
fn observe(block: &Arc<dyn BlockScheme>, arrived: bool) {
    {
        let mut pending = PENDING.lock();
        if pending.len() == pending.capacity() {
            // Perder el aviso es malo, pero pedir memoria desde aquí puede
            // colgar la máquina. Y si la cola está llena es que la tarea no
            // corre, con lo que crecerla no arreglaría nada.
            warn!("[hotplug] la cola de avisos está llena; se descarta uno");
            return;
        }
        pending.push((block.clone(), arrived));
    }
    let w = WAKER.lock().clone();
    if let Some(w) = w {
        w.wake();
    }
}

/// Crea los nodos de un disco que acaba de llegar.
///
/// Hace E/S: sólo desde la tarea.
fn add_nodes(block: Arc<dyn BlockScheme>) {
    if CREATED
        .lock()
        .iter()
        .any(|(b, _, _)| Arc::ptr_eq(b, &block))
    {
        // Dos avisos de alta del mismo disco sin baja por medio. No es un
        // error --un sondeo puede repetirse-- pero crear el nodo dos veces sí.
        return;
    }
    let (name, slot) = {
        let mut slots = SLOTS.lock();
        let n = next_free_name(&occupied(&slots), block.name());
        let slot = claim_slot(&mut slots, &n);
        (n, slot)
    };
    // Un hueco de 16 minors por ranura, como en el arranque, para que caben
    // las 15 particiones.
    let base_index = slot * 16;

    // El `Arc` se saca del cerrojo antes de tocar devfs: `lookup_virtual_fs`
    // coge este mismo cerrojo en cada acceso a `/dev`, y no hay razón para
    // tenerlo mientras se crean nodos y se leen sectores.
    let root = crate::fs::DEVFS_ROOT.lock().clone();
    let Some(root) = root else {
        warn!("[hotplug] {}: /dev no está montado todavía", name);
        release_slot(&mut SLOTS.lock(), &name);
        return;
    };
    let dev = Arc::new(BlockDev::new(base_index, block.clone(), name.clone()));
    if let Err(e) = root.add(&name, dev) {
        warn!("[hotplug] no se pudo crear /dev/{}: {:?}", name, e);
        release_slot(&mut SLOTS.lock(), &name);
        return;
    }
    warn!("[hotplug] /dev/{} dado de alta ({})", name, block.name());
    CREATED
        .lock()
        .push((block.clone(), name.clone(), base_index));

    // Y aquí la E/S: leer el MBR y la cabecera GPT. Lo que se monta de un
    // pendrive es casi siempre la partición, no el disco entero, así que sin
    // esto el hotplug seguiría sin servir de nada. Es la misma rutina que usa
    // el `BLKRRPART` de cualquier otro disco.
    if let Err(e) = crate::fs::rescan_partitions(&name, &block, base_index) {
        warn!(
            "[hotplug] /dev/{}: no se pudieron buscar particiones: {:?}",
            name, e
        );
    }
}

/// Quita los nodos de un disco que se fue, y deja libre su ranura.
fn remove_nodes(block: &Arc<dyn BlockScheme>) {
    let found = {
        let mut created = CREATED.lock();
        created
            .iter()
            .position(|(b, _, _)| Arc::ptr_eq(b, block))
            .map(|pos| created.remove(pos))
    };
    let Some((_, name, _)) = found else {
        // Un disco del arranque, o uno cuya alta nunca llegó a hacerse.
        return;
    };
    if let Some(root) = crate::fs::DEVFS_ROOT.lock().clone() {
        // Las particiones primero, por el mismo motivo que el nodo va antes que
        // la entrada de la lista de dispositivos: nada nuevo puede abrirlas
        // mientras se retira el disco de debajo.
        for part_num in 1..=MAX_PARTS {
            let _ = root.remove(&partition_node_name(&name, part_num));
        }
        match root.remove(&name) {
            Ok(()) => warn!("[hotplug] /dev/{} dado de baja", name),
            Err(e) => warn!("[hotplug] no se pudo quitar /dev/{}: {:?}", name, e),
        }
    }
    // La ranura se libera aunque el borrado falle: dejarla cogida gastaría una
    // letra y 16 minors por cada enchufe de la sesión.
    release_slot(&mut SLOTS.lock(), &name);
}

/// Vacía la cola. Devuelve cuántos avisos atendió.
fn drain() -> usize {
    // `drain(..)` y no `mem::take`: lo segundo deja en su sitio un vector nuevo
    // con capacidad cero, y el siguiente aviso tendría que pedir memoria en
    // contexto de interrupción, que es justo lo que `PENDING_CAP` evita.
    let work: Vec<_> = PENDING.lock().drain(..).collect();
    let n = work.len();
    for (block, arrived) in work {
        if arrived {
            add_nodes(block);
        } else {
            remove_nodes(&block);
        }
    }
    n
}

/// La tarea que crea y quita los nodos.
struct BlockHotplugFuture;

impl Future for BlockHotplugFuture {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        loop {
            // El waker se guarda ANTES de vaciar, no después: un aviso que
            // llegue mientras se crea un nodo tiene que encontrar a quién
            // despertar, o se queda en la cola sin que nadie vuelva por él.
            *WAKER.lock() = Some(cx.waker().clone());
            if drain() == 0 {
                return Poll::Pending;
            }
        }
    }
}

/// Engancha `/dev` a las altas y bajas de disco en caliente.
///
/// Se llama al final del recorrido del arranque, cuando los nodos de los discos
/// que ya estaban puestos existen: así un disco del arranque no se anuncia dos
/// veces, y el primer aviso que llega es de verdad una novedad.
pub fn install() {
    kernel_hal::drivers::install_block_hotplug_observer(observe);
    kernel_hal::thread::spawn(BlockHotplugFuture);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slots(v: &[Option<&str>]) -> Vec<Option<String>> {
        v.iter().map(|x| x.map(|n| n.to_string())).collect()
    }

    #[test]
    fn el_primer_pendrive_es_sda_y_el_siguiente_sdb() {
        assert_eq!(next_free_name(&[], "usbdisk-1"), "sda");
        assert_eq!(next_free_name(&["sda"], "usbdisk-2"), "sdb");
        assert_eq!(next_free_name(&["sda", "sdb"], "usbdisk-3"), "sdc");
    }

    #[test]
    fn el_nombre_que_se_libero_se_vuelve_a_usar() {
        // Con `sdb` desenchufado, el siguiente pendrive es `sdb`, no `sdc`:
        // contar discos en vez de mirar lo ocupado gastaría una letra por
        // enchufe hasta quedarse sin letras.
        assert_eq!(next_free_name(&["sda", "sdc"], "usbdisk-4"), "sdb");
    }

    #[test]
    fn un_disco_del_arranque_no_le_quita_el_nombre_al_pendrive() {
        // `nvme0n1` ocupado no estorba a la familia `sd`, y al revés tampoco.
        assert_eq!(next_free_name(&["nvme0n1"], "usbdisk-1"), "sda");
        assert_eq!(next_free_name(&["sda"], "nvme0"), "nvme0n1");
        assert_eq!(next_free_name(&["vda"], "usbdisk-1"), "sda");
        assert_eq!(next_free_name(&["sda"], "virtio-blk"), "vda");
    }

    #[test]
    fn con_la_familia_entera_ocupada_sigue_por_la_doble_letra() {
        let taken: Vec<String> = (0..26)
            .map(|i| alloc::format!("sd{}", (b'a' + i) as char))
            .collect();
        let refs: Vec<&str> = taken.iter().map(|s| s.as_str()).collect();
        assert_eq!(next_free_name(&refs, "usbdisk-1"), "sdaa");
    }

    #[test]
    fn la_ranura_de_un_disco_no_se_mueve_cuando_otro_se_va() {
        // El caso que rompía la cuenta por longitud de lista: con cuatro discos
        // y el tercero desenchufado, el siguiente tiene que coger el HUECO del
        // tercero, no la ranura del cuarto, que sigue puesto. Si no, dos nodos
        // acaban con el mismo número de minor.
        let mut sl = slots(&[Some("sda"), Some("sdb"), Some("sdc"), Some("sdd")]);
        release_slot(&mut sl, "sdc");
        assert_eq!(sl, slots(&[Some("sda"), Some("sdb"), None, Some("sdd")]));
        assert_eq!(claim_slot(&mut sl, "sdc"), 2);
        assert_eq!(
            sl,
            slots(&[Some("sda"), Some("sdb"), Some("sdc"), Some("sdd")])
        );
        // Y `sdd` no se ha movido: su minor sigue siendo el de la ranura 3.
        assert_eq!(sl.iter().position(|s| s.as_deref() == Some("sdd")), Some(3));
    }

    #[test]
    fn coger_dos_veces_el_mismo_nombre_da_la_misma_ranura() {
        // Es lo que hace `note_boot_disk` si se le llama dos veces, y lo que
        // pasaría con dos avisos de alta seguidos: una ranura, no dos.
        let mut sl: Vec<Option<String>> = Vec::new();
        assert_eq!(claim_slot(&mut sl, "sda"), 0);
        assert_eq!(claim_slot(&mut sl, "sda"), 0);
        assert_eq!(sl.len(), 1);
    }

    #[test]
    fn una_ranura_libre_se_reutiliza_antes_de_crecer() {
        let mut sl = slots(&[None, Some("sdb")]);
        assert_eq!(claim_slot(&mut sl, "sda"), 0);
        assert_eq!(sl.len(), 2, "no tenía que crecer habiendo un hueco");
    }

    #[test]
    fn lo_ocupado_salta_los_huecos() {
        let sl = slots(&[Some("sda"), None, Some("sdc")]);
        assert_eq!(occupied(&sl), alloc::vec!["sda", "sdc"]);
    }

    #[test]
    fn el_nombre_y_la_ranura_van_juntos_al_dar_de_alta() {
        // La combinación que usa `add_nodes`: pedir nombre libre y luego
        // ranura. Con `sda` y `sdc` puestos, al pendrive le toca `sdb` y la
        // ranura que dejó libre, o sea el minor 16 y no el 48.
        let mut sl = slots(&[Some("sda"), None, Some("sdc")]);
        let name = next_free_name(&occupied(&sl), "usbdisk-1");
        assert_eq!(name, "sdb");
        assert_eq!(claim_slot(&mut sl, &name) * 16, 16);
    }
}
