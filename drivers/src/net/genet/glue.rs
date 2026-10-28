//! El GENET y smoltcp: la mitad que decide **cuando** mover tramas.
//!
//! El driver de al lado ([`super`]) mueve bytes; esto es lo que el
//! kernel enchufa: el `Device` de smoltcp, el manejador de la interrupcion y
//! la media docena de respuestas de `NetScheme` que `ifconfig` y `/proc`
//! necesitan. La forma es la del `pcnet32`, que es la mas nueva de la casa: el
//! trabajo de la interrupcion va a un trabajo diferido con la tarjeta
//! enmascarada, y la contabilidad de direcciones y rutas la hace
//! [`ip_cfg`](crate::net::ip_cfg) en vez de estar copiada aqui.
//!
//! Lo que el GENET tiene de propio respecto a esos:
//!
//! 1. **La interrupcion llega por dos bloques `INTRL2` de set/clear**, no por
//!    un registro de estado y una mascara. Ver [`super::irq`].
//! 2. **El enlace avisa por interrupcion** (`UMAC_IRQ_LINK_EVENT`), asi que
//!    reajustar el MAC a la velocidad nueva es parte del camino de la
//!    interrupcion y no de un vigilante aparte.
//! 3. **La tarjeta se arma despues de los anillos**: encender rx/tx antes de
//!    que haya huecos de recepcion es pedirle al MAC que desborde su RBUF.

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

use super::rings::RX_BUF_LENGTH;
use super::*;
use crate::net::get_sockets;
use crate::net::{ip_cfg, timer_now_as_micros};
use crate::scheme::{NetScheme, NetStats, RouteInfo, Scheme};
use crate::{DeviceError, DeviceResult};
use core::sync::atomic::{AtomicBool, Ordering};
use lock::Mutex;

/// Lo que el GENET pone en el aire: 1514 bytes de trama Ethernet.
pub const GENET_MTU: usize = 1500;
/// El hueco de pila donde se arma una trama de salida. Cubre la MTU anunciada
/// con sitio de sobra; lo que no quepa se rechaza, nunca se recorta.
const TX_SCRATCH_LEN: usize = 1536;

/// La MAC que se usa si el cargador de arranque no dejo ninguna.
///
/// En la Raspberry Pi el firmware pone la MAC de la placa en los registros del
/// UMAC antes de entregar el control, y es la direccion que esta impresa en la
/// caja. Si no hay ninguna, inventar una **administrada localmente** (bit 1 del
/// primer byte) es lo unico honrado: una direccion global inventada puede
/// chocar con una Broadcom de verdad en la misma red.
const MAC_FALLBACK: [u8; 6] = [0x02, 0x00, 0x5E, 0x00, 0x53, 0x01];

/// Una MAC que no sirve para nada: todo ceros, o la de broadcast.
fn mac_is_usable(mac: &[u8; 6]) -> bool {
    let ceros = mac.iter().all(|b| *b == 0);
    let unos = mac.iter().all(|b| *b == 0xFF);
    // Y el bit de multicast puesto en una direccion de origen es invalido: una
    // trama con esa MAC de origen la tira el primer conmutador.
    let multicast = mac[0] & 1 != 0;
    !ceros && !unos && !multicast
}

// ---------------------------------------------------------------------------
// El Device de smoltcp
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct GenetDriver {
    pub rings: Arc<Mutex<GenetRings>>,
    /// La misma ventana de registros que los anillos, para lo que **no** puede
    /// esperar al cerrojo: enmascarar y reconocer la interrupcion. El cerrojo
    /// guarda el estado de los anillos (los cursores), no la ventana; un
    /// manejador que tuviera que cogerlo para enmascarar se quedaria esperando
    /// al trabajo diferido que esta usando los anillos, con la linea
    /// levantada.
    pub mac: GenetMac,
    pub stats: Arc<Mutex<NetStats>>,
    /// La direccion MDIO del PHY, para reajustar el enlace cuando avisa.
    pub phy_addr: u8,
    /// Si el RGMII de esta placa lleva el retardo puesto por el PHY
    /// (`rgmii-rxid`, que es lo que declara la Pi 4) y no por el MAC.
    pub id_mode_dis: bool,
}

pub struct GenetRxToken {
    data: Vec<u8>,
    stats: Arc<Mutex<NetStats>>,
}

pub struct GenetTxToken {
    driver: GenetDriver,
    stats: Arc<Mutex<NetStats>>,
}

impl GenetDriver {
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
            // `IoError` es una trama que el MAC marco como mala: el hueco ya
            // esta devuelto y la cuenta hecha. **No es una trama de longitud
            // cero**: devolverla como tal la daria por buena hasta el primer
            // intento de leerle la cabecera Ethernet.
            Err(_) => None,
        }
    }

    /// Reajusta el MAC a lo que diga el PHY. Devuelve el enlace leido.
    fn refresh_link(&self) -> Link {
        match self.mac.read_link(self.phy_addr) {
            Ok(link) => {
                if link.up {
                    if let Err(e) = self.mac.adjust_link(&link, self.id_mode_dis) {
                        warn!("genet: no se pudo ajustar el enlace: {:?}", e);
                    }
                    info!(
                        "genet: enlace a {} Mbps {}",
                        link.speed.mbps(),
                        if link.full_duplex { "full" } else { "half" }
                    );
                } else {
                    info!("genet: enlace caido");
                }
                link
            }
            Err(e) => {
                warn!("genet: no se pudo leer el PHY: {:?}", e);
                Link::DOWN
            }
        }
    }
}

impl phy::Device<'_> for GenetDriver {
    type RxToken = GenetRxToken;
    type TxToken = GenetTxToken;

    fn receive(&mut self) -> Option<(Self::RxToken, Self::TxToken)> {
        self.take_frame().map(|data| {
            (
                GenetRxToken {
                    data,
                    stats: self.stats.clone(),
                },
                GenetTxToken {
                    driver: self.clone(),
                    stats: self.stats.clone(),
                },
            )
        })
    }

    fn transmit(&mut self) -> Option<Self::TxToken> {
        if self.rings.lock().can_send() {
            Some(GenetTxToken {
                driver: self.clone(),
                stats: self.stats.clone(),
            })
        } else {
            None
        }
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = GENET_MTU + 14;
        // `max_burst_size` se queda sin poner: smoltcp recorta la ventana TCP
        // a `burst * MSS` por un u16, asi que un lote de 64 da la vuelta y
        // deja la conexion en unos pocos Mbps. La misma trampa esta
        // documentada en `e1000.rs` y en `pcnet32.rs`.
        caps.max_burst_size = None;
        caps.medium = phy::Medium::Ethernet;
        caps
    }
}

impl phy::RxToken for GenetRxToken {
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

impl phy::TxToken for GenetTxToken {
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
pub struct GenetInterface {
    iface: Arc<Mutex<Interface<'static, GenetDriver>>>,
    driver: GenetDriver,
    name: String,
    irq: usize,
    poll_pending: Arc<AtomicBool>,
    pub stats: Arc<Mutex<NetStats>>,
    pub routes: Arc<Mutex<Vec<RouteInfo>>>,
    pub ip_addrs: Arc<Mutex<Vec<IpCidr>>>,
}

impl GenetInterface {
    /// Vacia smoltcp **sin tocar la mascara**.
    ///
    /// El camino de la interrupcion desenmascara solo despues de bajar
    /// `poll_pending`; desenmascarar aqui dejaria que una trama que llega en
    /// esa ventana levante una interrupcion que encuentra `poll_pending`
    /// todavia puesto, reconoce la causa y no encola nada: la trama se queda
    /// en el anillo hasta que llegue la siguiente.
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
                    trace!("[genet] poll: {:?}", p);
                    Ok(())
                }
                Err(err) => {
                    warn!("[genet] poll got err {}", err);
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

impl Scheme for GenetInterface {
    fn name(&self) -> &str {
        "genet"
    }

    fn handle_irq(&self, irq: usize) {
        if irq != self.irq {
            return;
        }

        let status = self.driver.mac.irq_status();
        if status == 0 {
            // No es nuestra. La linea del GENET en la Pi 4 no se comparte, pero
            // una interrupcion espuria no debe reconocer nada: lo que se
            // reconoce sin atender es un evento perdido.
            return;
        }
        // Se reconoce lo que se va a atender, que es exactamente lo que se
        // acaba de leer.
        self.driver.mac.irq_ack(status);

        if self.poll_pending.load(Ordering::SeqCst) {
            // Ya hay un trabajo encolado y la tarjeta sigue enmascarada hasta
            // que corra. Desenmascarar aqui dejaria que los flancos siguientes
            // reconozcan causas que ese trabajo no va a ver.
            return;
        }
        self.poll_pending.store(true, Ordering::SeqCst);
        self.driver.mac.irq_mask(GENET_IRQ_WANTED);

        // El guardia se construye FUERA del cierre y se mete dentro, para que
        // la mascara se levante tanto si el trabajo corre como si se descarta
        // sin correr: `deferred_job` limita su cola y echa entradas cuando hay
        // presion, y bajar la bandera en el CUERPO del cierre dejaria la
        // tarjeta enmascarada y `poll_pending` puesto para siempre. Es la
        // forma en la que acabaron `e1000.rs`, `e1000e.rs` y `pcnet32.rs`.
        struct PollPendingGuard {
            pending: Arc<AtomicBool>,
            iface: GenetInterface,
        }
        impl Drop for PollPendingGuard {
            fn drop(&mut self) {
                // Primero la bandera y luego la mascara, para que una
                // interrupcion que llegue despues de desenmascarar encuentre
                // `poll_pending` bajado y encole un poll nuevo.
                self.pending.store(false, Ordering::SeqCst);
                self.iface.driver.mac.irq_unmask(GENET_IRQ_WANTED);
            }
        }
        let guard = PollPendingGuard {
            pending: self.poll_pending.clone(),
            iface: self.clone(),
        };
        let me = self.clone();
        crate::utils::deferred_job::push_deferred_job_front(move || {
            let guard = guard;
            // El enlace primero: si cambio la velocidad, el MAC tiene que
            // enterarse antes de que el poll de abajo le de tramas que poner
            // en el aire a la velocidad vieja.
            if status & UMAC_IRQ_LINK_EVENT != 0 {
                me.driver.refresh_link();
            }
            let _ = me.poll_inner();
            drop(guard);
        });
    }
}

impl NetScheme for GenetInterface {
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
        // El camino periodico lleva la mascara el mismo; el de la interrupcion
        // va por `poll_inner` y su guardia.
        let res = self.poll_inner();
        self.driver.mac.irq_unmask(GENET_IRQ_WANTED);
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
        // Las tramas que el MAC tiro antes de que ninguna capa de aqui las
        // viera: las que marco mal y las que descarto por no tener hueco. No
        // estan en ningun contador de este fichero porque nadie las recibio.
        let rings = self.driver.rings.lock();
        stats.rx_errors += rings.rx_errors();
        stats.rx_dropped += rings.rx_discarded() as u64;
        stats
    }

    fn get_mtu(&self) -> usize {
        GENET_MTU
    }
}

// ---------------------------------------------------------------------------
// El arranque
// ---------------------------------------------------------------------------

/// Levanta el GENET que vive en `base` y devuelve la interfaz.
///
/// `base` es **virtual**: la ventana tiene que venir ya mapeada sin cache, que
/// es lo que hace quien sonda el device tree. `phy_addr` es la direccion MDIO
/// del PHY si el device tree la declara; con `None` se barre el bus.
///
/// # Safety
///
/// `base` tiene que ser la ventana del GENET, de al menos 0x10000 bytes, y
/// nadie mas puede estar escribiendola.
pub unsafe fn genet_init(
    name: String,
    base: usize,
    irq: usize,
    phy_addr: Option<u8>,
    id_mode_dis: bool,
) -> DeviceResult<GenetInterface> {
    let mac = GenetMac::new(base);
    mac.check_supported()?;
    info!("genet: GENET v{} en {:#x}", mac.hw_version(), base);

    // Lo primero de todo, callar la tarjeta: puede venir del cargador de
    // arranque con interrupciones armadas y pendientes, y entonces la linea
    // esta levantada antes de que exista el manejador.
    mac.irq_disable_all();

    // La MAC se lee ANTES del reset: el reset del UMAC no la borra en este
    // silicio, pero leerla primero es lo que hace que no dependa de eso.
    let board_mac = mac.read_mac_addr();
    let hw_mac = if mac_is_usable(&board_mac) {
        board_mac
    } else {
        warn!(
            "genet: el firmware no dejo una MAC usable ({:02x?}), se inventa una local",
            board_mac
        );
        MAC_FALLBACK
    };

    mac.reset_umac();
    mac.set_port_mode_ext_gphy();
    mac.write_mac_addr(&hw_mac);

    let phy = match phy_addr {
        Some(addr) => addr,
        None => mac.phy_scan()?,
    };
    match mac.phy_id(phy) {
        Ok(id) if phy_is_bcm5421x(id) => {
            info!("genet: PHY BCM5421x ({:#010x}) en la direccion {}", id, phy)
        }
        Ok(id) => {
            // No se rechaza: el PHY habla MII generico, que es todo lo que
            // este driver le pide. Rechazar una placa por llevar otro PHY de
            // la familia seria dejarla sin red por un identificador.
            info!(
                "genet: PHY desconocido ({:#010x}) en la direccion {}",
                id, phy
            )
        }
        Err(e) => {
            warn!("genet: no se pudo identificar el PHY: {:?}", e);
            return Err(DeviceError::NotReady);
        }
    }

    // Los anillos antes de encender rx/tx: encender la recepcion sin huecos
    // donde poner tramas es pedirle al MAC que desborde su RBUF.
    let rings = GenetRings::new(GenetMac::new(base))?;

    // Y la renegociacion despues de los anillos pero antes de encender: el
    // enlace tarda segundos en subir, y lo que no puede pasar es que suba con
    // el MAC apagado y nadie vuelva a mirarlo. El `LINK_EVENT` del `INTRL2` es
    // el que lo recoge.
    if let Err(e) = mac.restart_aneg(phy) {
        warn!("genet: no se pudo renegociar el enlace: {:?}", e);
    }

    let stats = Arc::new(Mutex::new(NetStats::default()));
    let driver = GenetDriver {
        rings: Arc::new(Mutex::new(rings)),
        mac: GenetMac::new(base),
        stats: stats.clone(),
        phy_addr: phy,
        id_mode_dis,
    };
    // El enlace de ahora, si ya esta arriba. Si no, llegara por interrupcion.
    driver.refresh_link();

    mac.set_rx_tx_enabled(true);
    mac.irq_unmask(GENET_IRQ_WANTED);

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

    crate::klog_info!("genet interface {} discovered", name);
    Ok(GenetInterface {
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

#[cfg(test)]
mod glue_tests;
