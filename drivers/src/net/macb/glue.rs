//! El GEM del RP1 y smoltcp: la mitad que decide **cuando** mover tramas.
//!
//! El driver de al lado ([`super`]) mueve bytes; esto es lo que el kernel
//! enchufa: el `Device` de smoltcp, el manejador de la interrupcion y la media
//! docena de respuestas de `NetScheme` que `ifconfig` y `/proc` necesitan. La
//! forma es la del GENET de la Pi 4, que a su vez la saco del `pcnet32`: el
//! trabajo de la interrupcion va a un trabajo diferido con la tarjeta
//! enmascarada, y la contabilidad de direcciones y rutas la hace
//! [`ip_cfg`](crate::net::ip_cfg) en vez de estar copiada aqui.
//!
//! Lo que este MAC tiene de propio respecto al GENET:
//!
//! 1. **El enlace no avisa por interrupcion.** El GENET levanta
//!    `UMAC_IRQ_LINK_EVENT` y reajustar el MAC es parte del camino de la
//!    interrupcion. El GEM no: el `ISR_LINK` existe en el registro pero este
//!    MAC no lo levanta, porque el enlace lo lleva el PHY por MDIO. Asi que el
//!    enlace se mira en el `poll` periodico, que es lo que hace `phylink` con
//!    un PHY sin interrupcion.
//! 2. **El `ISR` puede limpiarse al leerlo**, asi que el estado se lee una
//!    sola vez por interrupcion y lo que devuelva es lo unico que se va a
//!    saber de ella. Ver [`super::irq`].
//! 3. **Los descriptores de transmision hay que recogerlos.** En el GENET el
//!    anillo de transmision se recoge por un indice del hardware; aqui cada
//!    descriptor lleva su bit, y si nadie los recoge la cola se llena y
//!    `can_send` dice no para siempre.
//!
//! **Nada de esto se ha probado contra el silicio**, y aqui no se puede: no
//! hay Pi 5, Eclipse no sabe levantar el PCIe del BCM2712 detras del que vive
//! el RP1, y el nodo `cdns,macb` puede llegar solo como overlay en caliente.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use smoltcp::iface::*;
use smoltcp::phy::{self, DeviceCapabilities};
use smoltcp::time::Instant;
use smoltcp::wire::*;
use smoltcp::Result;

use super::dt;
use super::*;
use crate::net::get_sockets;
use crate::net::{ip_cfg, timer_now_as_micros};
use crate::scheme::{NetScheme, NetStats, RouteInfo, Scheme};
use crate::utils::devicetree::Node;
use crate::{DeviceError, DeviceResult};
use core::sync::atomic::{AtomicBool, Ordering};
use lock::Mutex;

/// Lo que el GEM pone en el aire.
pub const MACB_MTU: usize = 1500;

/// El hueco de pila donde se arma una trama de salida. Cubre la MTU anunciada
/// con sitio de sobra; lo que no quepa se rechaza, nunca se recorta.
const TX_SCRATCH_LEN: usize = 1536;

/// La MAC que se usa si ni el arbol ni el firmware dejaron ninguna.
///
/// En la Pi 5 el firmware pone la MAC de la placa en los registros del GEM
/// antes de entregar el control, y es la direccion que esta impresa en la
/// caja. Si no hay ninguna, inventar una **administrada localmente** (bit 1
/// del primer byte) es lo unico honrado: una direccion global inventada puede
/// chocar con una de verdad en la misma red.
const MAC_FALLBACK: [u8; 6] = [0x02, 0x00, 0x5E, 0x00, 0x53, 0x02];

// ---------------------------------------------------------------------------
// El Device de smoltcp
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct MacbDriver {
    pub rings: Arc<Mutex<MacbRings>>,
    /// La misma ventana de registros que los anillos, para lo que **no** puede
    /// esperar al cerrojo: desarmar, reconocer y volver a armar la
    /// interrupcion. El cerrojo guarda el estado de los anillos (los
    /// cursores), no la ventana; un manejador que tuviera que cogerlo para
    /// desarmar se quedaria esperando al trabajo diferido que esta usando los
    /// anillos, con la linea levantada.
    pub mac: MacbMac,
    pub stats: Arc<Mutex<NetStats>>,
    /// La direccion MDIO del PHY, que salio del barrido del bus: el arbol del
    /// RP1 no la dice en ningun sitio.
    pub phy_addr: u8,
}

pub struct MacbRxToken {
    data: Vec<u8>,
    stats: Arc<Mutex<NetStats>>,
}

pub struct MacbTxToken {
    driver: MacbDriver,
    stats: Arc<Mutex<NetStats>>,
}

impl MacbDriver {
    /// Saca una trama del anillo, si hay.
    ///
    /// Una sola toma del cerrojo para la pregunta y para la respuesta: con dos
    /// otra CPU se lleva la trama en medio y `recv` camina un anillo cuyo
    /// cursor se ha movido.
    fn take_frame(&self) -> Option<Vec<u8>> {
        let mut rings = self.rings.lock();
        if !rings.can_recv() {
            return None;
        }
        let mut buf = vec![0u8; RX_BUF_LENGTH];
        match rings.recv(&mut buf) {
            Ok(n) => {
                buf.truncate(n);
                Some(buf)
            }
            // `IoError` es una trama que el MAC marco como mala, o una partida
            // en varios descriptores: el hueco ya esta devuelto y la cuenta
            // hecha. **No es una trama de longitud cero**: devolverla como tal
            // la daria por buena hasta el primer intento de leerle la cabecera
            // Ethernet.
            Err(_) => None,
        }
    }

    /// Recoge los descriptores de transmision que el MAC ya ha soltado.
    ///
    /// Esto **no existe en el GENET**, donde el anillo de transmision se
    /// recoge por un indice del hardware. Aqui cada descriptor lleva su bit, y
    /// si nadie los recoge la cola se llena y `can_send` dice no para siempre:
    /// la tarjeta se queda muda sin dar ningun error.
    fn reclaim(&self) {
        let recogidos = self.rings.lock().reclaim_tx();
        if recogidos > 0 {
            trace!("[macb] recogidos {} descriptores de transmision", recogidos);
        }
    }

    /// Reajusta el MAC a lo que diga el PHY. Devuelve el enlace leido.
    ///
    /// En el GENET esto cuelga de la interrupcion del enlace. Aqui no hay tal
    /// interrupcion, asi que esto lo llama el `poll` periodico: es lo que hace
    /// `phylink` con un PHY que no avisa.
    fn refresh_link(&self) -> Link {
        match self.mac.read_link(self.phy_addr) {
            Ok(link) => {
                // Lo que decide este `if` es **el registro**, no el MAC:
                // `adjust_link` rechaza por su cuenta un enlace caido
                // (`a_link_that_is_down_is_refused_rather_than_programmed`),
                // asi que sin el la velocidad tampoco se programaria. Lo que
                // cambiaria es que cada poll con el cable fuera dejaria un
                // aviso en el registro, una vez por tick.
                if link.up {
                    if let Err(e) = self.mac.adjust_link(&link) {
                        warn!("macb: no se pudo ajustar el enlace: {:?}", e);
                    }
                } else {
                    trace!("macb: enlace caido");
                }
                link
            }
            Err(e) => {
                warn!("macb: no se pudo leer el PHY: {:?}", e);
                Link::DOWN
            }
        }
    }
}

impl phy::Device<'_> for MacbDriver {
    type RxToken = MacbRxToken;
    type TxToken = MacbTxToken;

    fn receive(&mut self) -> Option<(Self::RxToken, Self::TxToken)> {
        self.take_frame().map(|data| {
            (
                MacbRxToken {
                    data,
                    stats: self.stats.clone(),
                },
                MacbTxToken {
                    driver: self.clone(),
                    stats: self.stats.clone(),
                },
            )
        })
    }

    fn transmit(&mut self) -> Option<Self::TxToken> {
        // Recoger **antes** de preguntar: la cola tiene 64 huecos menos el
        // centinela, y una tanda de salida los gasta de sobra. Si se pregunta
        // primero, el `None` corta el lote por descriptores que el MAC ya
        // habia soltado.
        self.reclaim();
        if self.rings.lock().can_send() {
            Some(MacbTxToken {
                driver: self.clone(),
                stats: self.stats.clone(),
            })
        } else {
            None
        }
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = MACB_MTU + 14;
        // `max_burst_size` se queda sin poner: smoltcp recorta la ventana TCP
        // a `burst * MSS` por un u16, asi que un lote de 64 da la vuelta y
        // deja la conexion en unos pocos Mbps. La misma trampa esta
        // documentada en `e1000.rs`, en `pcnet32.rs` y en el GENET.
        caps.max_burst_size = None;
        caps.medium = phy::Medium::Ethernet;
        caps
    }
}

impl phy::RxToken for MacbRxToken {
    fn consume<R, F>(mut self, _timestamp: Instant, f: F) -> Result<R>
    where
        F: FnOnce(&mut [u8]) -> Result<R>,
    {
        {
            let mut stats = self.stats.lock();
            stats.rx_packets += 1;
            stats.rx_bytes += self.data.len() as u64;
        }
        // Los oyentes de AF_PACKET, que es como el cliente de DHCP ve las
        // tramas.
        crate::net::net_defer_packet(&self.data);
        f(&mut self.data)
    }
}

impl phy::TxToken for MacbTxToken {
    fn consume<R, F>(self, _timestamp: Instant, len: usize, f: F) -> Result<R>
    where
        F: FnOnce(&mut [u8]) -> Result<R>,
    {
        // `len` viene de la capa IP, que esta smoltcp no recorta a la MTU del
        // dispositivo y no fragmenta: indexar un array de pila con ella tumba
        // el kernel con un solo `sendto` grande. Se rechaza.
        if len > TX_SCRATCH_LEN {
            self.stats.lock().tx_errors += 1;
            return Err(smoltcp::Error::Exhausted);
        }
        let mut buffer = [0u8; TX_SCRATCH_LEN];
        let result = f(&mut buffer[..len])?;

        let enviada = self.driver.rings.lock().send(&buffer[..len]).is_ok();
        if !enviada {
            // **Nunca** dar por enviada una trama que se tiro. smoltcp emite
            // el ACK de un segmento recibido por el TxToken emparejado con su
            // RxToken, y ya ha avanzado `remote_last_ack`: un `Ok` aqui pierde
            // ese ACK para siempre y el otro extremo se queda esperando una
            // ventana que cree cerrada. `Exhausted` es la respuesta para la
            // que smoltcp esta hecho: corta el lote y se queda el segmento.
            self.stats.lock().tx_errors += 1;
            return Err(smoltcp::Error::Exhausted);
        }

        let mut stats = self.stats.lock();
        stats.tx_packets += 1;
        stats.tx_bytes += len as u64;
        Ok(result)
    }
}

// ---------------------------------------------------------------------------
// La interfaz
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct MacbInterface {
    iface: Arc<Mutex<Interface<'static, MacbDriver>>>,
    driver: MacbDriver,
    name: String,
    irq: usize,
    poll_pending: Arc<AtomicBool>,
    pub stats: Arc<Mutex<NetStats>>,
    pub routes: Arc<Mutex<Vec<RouteInfo>>>,
    pub ip_addrs: Arc<Mutex<Vec<IpCidr>>>,
}

impl MacbInterface {
    /// Vacia smoltcp **sin tocar la mascara**.
    ///
    /// El camino de la interrupcion vuelve a armar solo despues de bajar
    /// `poll_pending`; armar aqui dejaria que una trama que llega en esa
    /// ventana levante una interrupcion que encuentra `poll_pending` todavia
    /// puesto, reconoce la causa y no encola nada: la trama se queda en el
    /// anillo hasta que llegue la siguiente.
    fn poll_inner(&self) -> DeviceResult {
        let timestamp = Instant::from_micros(timer_now_as_micros() as i64);
        let sockets = get_sockets();
        // `try_lock`, no una toma que bloquea. Esto corre desde un trabajo
        // diferido, y una llamada de socket vacia esa cola MIENTRAS tiene el
        // conjunto de sockets cogido: en este kernel con las interrupciones
        // apagadas, una toma que bloquea se queda girando sobre un cerrojo que
        // ya tiene esta misma CPU. Es el bloqueo que `e1000.rs` cuenta haberse
        // comido en una descarga de 64 MB. Quien lo tenga cogido hace el poll
        // al soltarlo, asi que saltarselo no pierde nada.
        let res = match (sockets.try_lock(), self.iface.try_lock()) {
            (Some(mut sockets), Some(mut iface)) => match iface.poll(&mut sockets, timestamp) {
                Ok(p) => {
                    trace!("[macb] poll: {:?}", p);
                    Ok(())
                }
                Err(err) => {
                    warn!("[macb] poll got err {}", err);
                    Err(DeviceError::IoError)
                }
            },
            _ => Ok(()),
        };
        crate::net::net_flush_deferred_packets();
        crate::net::wake_net_rx_waiters();
        res
    }
}

impl Scheme for MacbInterface {
    fn name(&self) -> &str {
        "macb"
    }

    fn handle_irq(&self, irq: usize) {
        if irq != self.irq {
            return;
        }

        // **Una sola lectura.** Con `GEM_IRQCOR` puesto, leer el `ISR` es lo
        // que lo limpia, asi que esto es todo lo que se va a saber de esta
        // interrupcion.
        let status = self.driver.mac.irq_status();
        if status == 0 {
            // No es nuestra. Una interrupcion espuria no debe reconocer nada:
            // lo que se reconoce sin atender es un evento perdido.
            return;
        }
        // Se reconoce lo que se va a atender, que es exactamente lo que se
        // acaba de leer. En el silicio que se limpia al leerlo esto no escribe
        // nada, y no hace falta.
        self.driver.mac.irq_ack(status);

        if status & ISR_HRESP != 0 {
            // El bus le contesto que no al DMA. Esto **no** es un problema de
            // red: es el DMA leyendo o escribiendo donde no debe, y lo unico
            // honrado es decirlo. Linux reinicia la tarjeta entera; aqui no
            // hay con que, asi que se cuenta y se sigue.
            warn!("macb: el bus rechazo un acceso del DMA (HRESP)");
            self.stats.lock().rx_errors += 1;
        }

        if self.poll_pending.load(Ordering::SeqCst) {
            // Ya hay un trabajo encolado y la tarjeta sigue desarmada hasta
            // que corra. Armar aqui dejaria que los flancos siguientes
            // reconozcan causas que ese trabajo no va a ver.
            return;
        }
        self.poll_pending.store(true, Ordering::SeqCst);
        self.driver.mac.irq_disable(MACB_IRQ_WANTED);

        // El guardia se construye FUERA del cierre y se mete dentro, para que
        // la mascara se levante tanto si el trabajo corre como si se descarta
        // sin correr: `deferred_job` limita su cola y echa entradas cuando hay
        // presion, y bajar la bandera en el CUERPO del cierre dejaria la
        // tarjeta desarmada y `poll_pending` puesto para siempre. Es la forma
        // en la que acabaron `e1000.rs`, `pcnet32.rs` y el GENET.
        struct PollPendingGuard {
            pending: Arc<AtomicBool>,
            iface: MacbInterface,
        }
        impl Drop for PollPendingGuard {
            fn drop(&mut self) {
                // Primero la bandera y luego la mascara, para que una
                // interrupcion que llegue despues de armar encuentre
                // `poll_pending` bajado y encole un poll nuevo.
                //
                // **El orden de estas dos lineas no esta cubierto por ningun
                // test**, y no se puede estar: lo que las distingue es un
                // flanco que caiga justo entre las dos, y aqui no hay con que
                // meter uno. Al revés, ese flanco encuentra la tarjeta armada
                // y `poll_pending` todavia puesto, reconoce su causa y no
                // encola nada: la trama se queda en el anillo hasta que llegue
                // la siguiente. Que el estado final sea el mismo es lo que
                // hace que esto solo se note bajo carga.
                self.pending.store(false, Ordering::SeqCst);
                self.iface.driver.mac.irq_enable(MACB_IRQ_WANTED);
            }
        }
        let guard = PollPendingGuard {
            pending: self.poll_pending.clone(),
            iface: self.clone(),
        };
        let me = self.clone();
        crate::utils::deferred_job::push_deferred_job_front(move || {
            let guard = guard;
            // Recoger antes del poll: lo que el poll quiera transmitir
            // necesita huecos, y los huecos los libera esto.
            if status & MACB_TX_INT_FLAGS != 0 {
                me.driver.reclaim();
            }
            let _ = me.poll_inner();
            drop(guard);
        });
    }
}

impl NetScheme for MacbInterface {
    fn get_mac(&self) -> EthernetAddress {
        self.iface.lock().ethernet_addr()
    }

    fn ifname(&self) -> &str {
        &self.name
    }

    fn is_loopback(&self) -> bool {
        false
    }

    fn get_ifname(&self) -> String {
        self.name.clone()
    }

    fn get_ip_address(&self) -> Vec<IpCidr> {
        self.ip_addrs.lock().clone()
    }

    fn seed_neighbor(&self, protocol: IpAddress, hardware: EthernetAddress) -> DeviceResult {
        let timestamp = Instant::from_micros(timer_now_as_micros() as i64);
        ip_cfg::seed_neighbor(&mut self.iface.lock(), protocol, hardware, timestamp)
    }

    fn poll(&self) -> DeviceResult {
        // **Aqui se mira el enlace**, que es lo que el GENET hace en el camino
        // de la interrupcion: este MAC no levanta `ISR_LINK`, porque el enlace
        // lo lleva el PHY. Primero el enlace y luego el poll, para que el MAC
        // este a la velocidad nueva antes de que le lleguen tramas que poner
        // en el aire.
        self.driver.refresh_link();
        self.driver.reclaim();
        // El camino periodico lleva la mascara el mismo; el de la interrupcion
        // va por `poll_inner` y su guardia.
        let res = self.poll_inner();
        self.driver.mac.irq_enable(MACB_IRQ_WANTED);
        res
    }

    fn recv(&self, buf: &mut [u8]) -> DeviceResult<usize> {
        match self.driver.take_frame() {
            Some(pkt) => {
                let n = pkt.len().min(buf.len());
                buf[..n].copy_from_slice(&pkt[..n]);
                Ok(n)
            }
            None => Err(DeviceError::NotReady),
        }
    }

    fn send(&self, data: &[u8]) -> DeviceResult<usize> {
        self.driver.reclaim();
        let mut rings = self.driver.rings.lock();
        if !rings.can_send() {
            return Err(DeviceError::NotReady);
        }
        rings.send(data)
    }

    fn can_recv(&self) -> bool {
        self.driver.rings.lock().can_recv()
    }

    fn can_send(&self) -> bool {
        self.driver.reclaim();
        self.driver.rings.lock().can_send()
    }

    fn set_ipv4_address(&self, cidr: Ipv4Cidr) -> DeviceResult {
        ip_cfg::set_ipv4_address(&mut self.iface.lock(), &mut self.ip_addrs.lock(), cidr)
    }

    fn add_ip_address(&self, cidr: IpCidr) -> DeviceResult {
        ip_cfg::add_ip_address(&mut self.iface.lock(), &mut self.ip_addrs.lock(), cidr)
    }

    fn remove_ip_address(&self, cidr: IpCidr) -> DeviceResult {
        ip_cfg::remove_ip_address(&mut self.iface.lock(), &mut self.ip_addrs.lock(), cidr)
    }

    fn add_route(&self, cidr: IpCidr, gateway: Option<IpAddress>) -> DeviceResult {
        ip_cfg::add_route(
            &mut self.iface.lock(),
            &mut self.routes.lock(),
            cidr,
            gateway,
        )
    }

    fn del_route(&self, cidr: IpCidr, _gateway: Option<IpAddress>) -> DeviceResult {
        ip_cfg::del_route(&mut self.iface.lock(), &mut self.routes.lock(), cidr)
    }

    fn get_routes(&self) -> Vec<RouteInfo> {
        let iface = self.iface.lock();
        ip_cfg::reported_routes(&self.routes.lock(), iface.ip_addrs())
    }

    fn get_stats(&self) -> NetStats {
        let mut stats = self.stats.lock().clone();
        // Lo que el MAC tiro antes de que ninguna capa de aqui lo viera: las
        // tramas que marco mal o partio, y las transmisiones que dijo haber
        // fallado. No estan en ningun contador de este fichero porque nadie
        // las recibio ni las dio por enviadas.
        let rings = self.driver.rings.lock();
        stats.rx_errors += rings.rx_errors();
        stats.tx_errors += rings.tx_errors();
        stats
    }

    fn get_mtu(&self) -> usize {
        MACB_MTU
    }
}

// ---------------------------------------------------------------------------
// El arranque
// ---------------------------------------------------------------------------

/// Levanta el GEM que vive en `base` y devuelve la interfaz.
///
/// `base` es **virtual**: la ventana tiene que venir ya mapeada sin cache, que
/// es lo que hace quien sonda el device tree. `dt_mac` es la MAC que trae el
/// arbol, que manda sobre la que haya en los registros; en la Pi 5 lo normal
/// es que sea `None`, porque el arbol trae seis ceros.
///
/// **No hay parametro de direccion de PHY**, al contrario que en el GENET: el
/// nodo del RP1 no la dice en ningun sitio, asi que siempre se barre el bus.
///
/// # Safety
///
/// `base` tiene que ser la ventana del GEM, de al menos
/// [`MACB_WINDOW_BYTES`] bytes, y nadie mas puede estar escribiendola.
pub unsafe fn macb_init(
    name: String,
    base: usize,
    irq: usize,
    cfg: MacbConfig,
    dt_mac: Option<[u8; 6]>,
) -> DeviceResult<MacbInterface> {
    let mac = MacbMac::new(base);
    mac.check_supported()?;
    let (idnum, rev) = mac.hw_id();
    info!(
        "macb: Cadence GEM idnum {} rev {:#06x} en {:#x}",
        idnum, rev, base
    );

    // Callar la tarjeta --- puede venir del cargador de arranque con
    // interrupciones armadas y pendientes, y entonces la linea esta levantada
    // antes de que exista el manejador --- lo hace `reset_hw` unas lineas mas
    // abajo, y aqui **no se repite**: entre esto y el reset solo se leen
    // registros, asi que una segunda llamada no cambiaria nada que se pueda
    // observar. Queda dicho porque es lo primero que se busca al leer un
    // arranque de driver, y no estar no es lo mismo que haberse olvidado.

    // La MAC, por orden: la que trae el arbol, la que dejo el firmware en los
    // registros, y una inventada.
    //
    // Se lee **antes** del reset: el reset del GEM no borra los juegos de
    // direccion en este silicio, pero leerla primero es lo que hace que no
    // dependa de eso.
    let hw_mac = dt_mac.or_else(|| mac.hw_mac_addr()).unwrap_or_else(|| {
        warn!("macb: ni el arbol ni el firmware dejaron una MAC usable; se inventa una local");
        MAC_FALLBACK
    });

    mac.reset_hw();
    mac.init_config(&cfg);
    // El puerto de gestion, sin el cual el MDIO devuelve basura **sin dar
    // error**. Va despues del `init_config`, que escribe el `NCFGR` entero
    // (el divisor del MDC esta ahi) y antes del barrido.
    mac.set_mdio_enabled(true);
    mac.write_mac_addr(&hw_mac);

    // El barrido es el unico camino: el arbol del RP1 no dice donde esta el
    // PHY. Un GEM sin PHY detras es una tarjeta sin enlace, asi que esto si es
    // motivo para no seguir.
    let phy = mac.phy_scan()?;
    match mac.phy_id(phy) {
        Ok(id) => info!("macb: PHY {:#010x} en la direccion {}", id, phy),
        Err(e) => {
            warn!("macb: no se pudo identificar el PHY: {:?}", e);
            return Err(DeviceError::NotReady);
        }
    }

    // Los anillos antes de encender rx/tx: encender la recepcion sin huecos
    // donde poner tramas es pedirle al MAC que desborde su FIFO. Y esto es lo
    // que rechaza arrancar si el DMA no alcanza a las regiones.
    let rings = MacbRings::new(MacbMac::new(base))?;

    // Y la renegociacion despues de los anillos pero antes de encender: el
    // enlace tarda segundos en subir, y lo que no puede pasar es que suba con
    // el MAC apagado y nadie vuelva a mirarlo. Aqui no hay interrupcion del
    // enlace que lo recoja --- lo recoge el `poll` periodico --- asi que esto
    // es lo unico que arranca la negociacion.
    if let Err(e) = mac.restart_aneg(phy) {
        warn!("macb: no se pudo renegociar el enlace: {:?}", e);
    }

    let stats = Arc::new(Mutex::new(NetStats::default()));
    let driver = MacbDriver {
        rings: Arc::new(Mutex::new(rings)),
        mac: MacbMac::new(base),
        stats: stats.clone(),
        phy_addr: phy,
    };
    // El enlace de ahora, si ya esta arriba. Si no, lo cogera el `poll`.
    let link = driver.refresh_link();
    if link.up {
        info!(
            "macb: enlace a {} Mbps {}",
            link.speed.mbps(),
            if link.full_duplex { "full" } else { "half" }
        );
    }

    mac.set_rx_tx_enabled(true);
    mac.irq_enable(MACB_IRQ_WANTED);

    let ethernet_addr = EthernetAddress::from_bytes(&hw_mac);
    let mut eui64 = [0u8; 8];
    eui64[0] = hw_mac[0] ^ 2;
    eui64[1] = hw_mac[1];
    eui64[2] = hw_mac[2];
    eui64[3] = 0xff;
    eui64[4] = 0xfe;
    eui64[5] = hw_mac[3];
    eui64[6] = hw_mac[4];
    eui64[7] = hw_mac[5];
    let link_local = Ipv6Address::new(
        0xfe80,
        0,
        0,
        0,
        (eui64[0] as u16) << 8 | eui64[1] as u16,
        (eui64[2] as u16) << 8 | eui64[3] as u16,
        (eui64[4] as u16) << 8 | eui64[5] as u16,
        (eui64[6] as u16) << 8 | eui64[7] as u16,
    );
    // Sin direccion IPv4: la pone el DHCP o `ifconfig`. Clavar una aqui es lo
    // que hacen los drivers viejos de la casa y es como una placa acaba
    // respondiendo en una direccion que nadie le dio.
    let ip_addrs = vec![
        IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0),
        IpCidr::Ipv6(Ipv6Cidr::new(link_local, 64)),
        IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0),
        IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0),
    ];
    // Almacenamiento de rutas **por tarjeta**: un `static mut` compartido se
    // solaparia si se sondan dos, y corromperia las dos tablas.
    let routes_storage: &'static mut [Option<(IpCidr, Route)>] =
        Box::leak(vec![None; 4].into_boxed_slice());
    let iface = InterfaceBuilder::new(driver.clone())
        .ethernet_addr(ethernet_addr)
        .neighbor_cache(NeighborCache::new(BTreeMap::new()))
        .ip_addrs(ip_addrs.clone())
        .routes(Routes::new(routes_storage))
        .finalize();

    crate::klog_info!("macb interface {} discovered", name);
    Ok(MacbInterface {
        iface: Arc::new(Mutex::new(iface)),
        driver,
        name,
        irq,
        poll_pending: Arc::new(AtomicBool::new(false)),
        stats,
        routes: Arc::new(Mutex::new(vec![])),
        ip_addrs: Arc::new(Mutex::new(ip_addrs)),
    })
}

/// Levanta el GEM que describe `node`, leyendole al device tree lo que sabe.
///
/// Es lo que la rama de `parse_ethernet` llama, y esta aqui y no alli para que
/// se pueda probar: alli el `base` sale del mapeador y no hay forma de ponerle
/// un MAC falso debajo.
///
/// # Safety
///
/// Lo mismo que [`macb_init`]: `base` tiene que ser la ventana del GEM.
pub unsafe fn macb_from_dt(
    name: String,
    node: &Node,
    base: usize,
    irq: usize,
) -> DeviceResult<MacbInterface> {
    if !dt::phy_mode_is_supported(node) {
        return Err(DeviceError::NotSupported);
    }
    macb_init(name, base, irq, dt::config(node), dt::mac_addr(node))
}

#[cfg(test)]
mod glue_tests;
