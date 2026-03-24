//! Network Service - Manages network stack using smoltcp
//!
//! This service manages network connectivity using the smoltcp stack.
//! It talks to the kernel via the eth: scheme for raw packet I/O.
//!
//! # Arquitectura del Super-Bucle
//!
//! El servicio usa la arquitectura de "super-bucle" para bare-metal: la función
//! `network_task()` se ejecuta continuamente en el bucle principal. En cada
//! iteración:
//!
//! 1. Se obtiene el tiempo monotónico (`get_now_ms`) — **CRÍTICO**: smoltcp
//!    lo usa para el exponential backoff del DHCP (2s, 4s, 8s, 16s...).
//! 2. Se hace `iface.poll(timestamp, device, sockets)` para procesar paquetes.
//! 3. Se gestionan los eventos DHCP (Configured, Deconfigured).
//! 4. Se detectan transiciones de enlace y el watchdog DHCP.
//!
//! El `sleep_ms(1)` al final del bucle evita consumir el 100% de la CPU.

extern crate alloc;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use std::prelude::v1::*;
use eclipse_libc::{getpid, sleep_ms, ioctl, O_RDWR};
use eclipse_libc::{send_ipc, receive_ipc, eclipse_open, eclipse_read, eclipse_write};
use eclipse_libc::{get_system_stats, SystemStats};
use smoltcp::phy::{self, DeviceCapabilities, Medium, Loopback};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, IpAddress, IpCidr, Ipv4Address, Ipv6Address, DnsQueryType, Ipv4Cidr};
use smoltcp::iface::{Config, Interface, SocketSet};
use smoltcp::socket::dns::{Socket as DnsSocket};
use std::collections::BTreeMap;

mod net_ipc;
use net_ipc::*;

#[derive(Clone, Copy)]
struct DhcpInfo {
    address: Ipv4Cidr,
    router: Option<Ipv4Address>,
    dns_server: Option<Ipv4Address>,
}

/// Raw Ethernet device using the kernel's eth: scheme
struct RawEthernetDevice {
    fd: usize,
    mac: EthernetAddress,
    rx_bytes: Arc<AtomicU64>,
    tx_bytes: Arc<AtomicU64>,
}

impl RawEthernetDevice {
    fn new(interface_id: usize) -> Option<Self> {
        let path = format!("eth:{}", interface_id);
        let fd = eclipse_open(&path, O_RDWR, 0);
        if fd < 0 {
            return None;
        }
        
        let mut mac_bytes = [0u8; 6];
        let _ = unsafe { ioctl(fd as i32, 0x8001, mac_bytes.as_mut_ptr() as *mut _) };
        let mac = EthernetAddress(mac_bytes);
        
        Some(RawEthernetDevice {
            fd: fd as usize,
            mac,
            rx_bytes: Arc::new(AtomicU64::new(0)),
            tx_bytes: Arc::new(AtomicU64::new(0)),
        })
    }

    fn get_link_status(&self) -> bool {
        let mut link_info = [0u8; 12];
        if unsafe { ioctl(self.fd as i32, 0x8002, link_info.as_mut_ptr() as *mut _) } == 0 {
            link_info[0] != 0
        } else {
            true // assume up if ioctl fails for backward compatibility
        }
    }
}

impl phy::Device for RawEthernetDevice {
    type RxToken<'a> = RxToken;
    type TxToken<'a> = TxToken;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let mut buffer = [0u8; 1520];
        let len = eclipse_read(self.fd as u32, &mut buffer);
        if len > 0 {
            self.rx_bytes.fetch_add(len as u64, Ordering::Relaxed);
            println!("[NETWORK-SERVICE] eth:0 received packet: {} bytes", len);
            let mut data = vec![0u8; len as usize];
            data.copy_from_slice(&buffer[..len as usize]);
            Some((RxToken { data }, TxToken { fd: self.fd, tx_bytes: self.tx_bytes.clone() }))
        } else {
            None
        }
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(TxToken { fd: self.fd, tx_bytes: self.tx_bytes.clone() })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = 1514;
        caps.medium = Medium::Ethernet;
        caps
    }
}

struct RxToken {
    data: Vec<u8>,
}

impl phy::RxToken for RxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.data)
    }
}

struct TxToken {
    fd: usize,
    tx_bytes: Arc<AtomicU64>,
}

impl phy::TxToken for TxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buffer = vec![0u8; len];
        let result = f(&mut buffer);
        println!("[NETWORK-SERVICE] eth:0 sending packet: {} bytes", len);
        let written = eclipse_write(self.fd as u32, &buffer);
        if written < 0 {
            println!(
                "[NETWORK-SERVICE] ERROR: eth:0 write failed while sending packet (len={})",
                len
            );
        } else if written as usize != len {
            println!(
                "[NETWORK-SERVICE] WARN: eth:0 partial write (sent={} expected={})",
                written,
                len
            );
            self.tx_bytes.fetch_add(written as u64, Ordering::Relaxed);
        } else {
            self.tx_bytes.fetch_add(len as u64, Ordering::Relaxed);
        }
        result
    }
}

/// Obtiene los milisegundos desde el arranque del sistema.
/// CRÍTICO: smoltcp depende de esto para saber cuándo reintentar DHCP (exponential backoff).
fn get_now_ms() -> u64 {
    let mut stats = SystemStats::default();
    if unsafe { get_system_stats(&mut stats) } >= 0 {
        stats.uptime_ticks
    } else {
        0
    }
}

/// Tarea de red: debe ejecutarse continuamente en el bucle principal.
/// Hace poll a la pila, procesa paquetes y gestiona los eventos DHCP.
/// La máquina de estado de smoltcp con exponential backoff (2s, 4s, 8s, 16s...)
/// funciona correctamente mientras este task reciba un timestamp que avance en tiempo real.
fn network_task(
    device: &mut RawEthernetDevice,
    iface: &mut Interface,
    sockets: &mut SocketSet<'_>,
    lo_iface: &mut Interface,
    lo_device: &mut Loopback,
    lo_sockets: &mut SocketSet<'_>,
    dhcp_handle: smoltcp::iface::SocketHandle,
    current_dhcp_config: &mut Option<DhcpInfo>,
    dhcp_deconfigured_since: &mut Option<u64>,
    prev_link_up: &mut bool,
    use_dhcp: bool,
) {
    // 1. Obtener tiempo monotónico del sistema (CRÍTICO para reintentos DHCP)
    let uptime_ms = get_now_ms();
    let timestamp = Instant::from_millis(uptime_ms as i64);

    // 2. Poll a loopback y a la interfaz Ethernet
    lo_iface.poll(timestamp, lo_device, lo_sockets);
    iface.poll(timestamp, device, sockets);

    let link_up = device.get_link_status();

    // 3. Detectar transición DOWN → UP: forzar DHCP reset inmediato
    // (smoltcp podría estar en backoff largo de intentos anteriores)
    if link_up && !*prev_link_up {
        println!("[NETWORK-SERVICE] Physical Link UP detected on eth0");
        if use_dhcp {
            println!("[NETWORK-SERVICE]   Forcing DHCP reset for immediate discovery");
            sockets.get_mut::<smoltcp::socket::dhcpv4::Socket>(dhcp_handle).reset();
            *current_dhcp_config = None;
            *dhcp_deconfigured_since = Some(uptime_ms);
            iface.update_ip_addrs(|addrs| {
                addrs.retain(|a| matches!(a.address(), smoltcp::wire::IpAddress::Ipv6(_)));
            });
            iface.routes_mut().remove_default_ipv4_route();
        }
    }
    *prev_link_up = link_up;

    // 4. Procesar eventos DHCP de smoltcp
    if use_dhcp {
        let event = sockets.get_mut::<smoltcp::socket::dhcpv4::Socket>(dhcp_handle).poll();
        match event {
            None => {}
            Some(smoltcp::socket::dhcpv4::Event::Configured(config)) => {
                *current_dhcp_config = Some(DhcpInfo {
                    address: config.address,
                    router: config.router,
                    dns_server: config.dns_servers.first().copied(),
                });
                *dhcp_deconfigured_since = None;
                let info = current_dhcp_config.as_ref().unwrap();
                println!("[NETWORK-SERVICE] eth0 configured via DHCPv4:");
                println!("  IP address:      {}", info.address);
                if let Some(router) = info.router {
                    println!("  Default gateway: {}", router);
                    if iface.routes_mut().add_default_ipv4_route(router).is_err() {
                        println!("[NETWORK-SERVICE] WARN: routing table full, default route not set");
                    }
                }
                if let Some(dns_server) = info.dns_server {
                    println!("  DNS server:      {}", dns_server);
                }
                iface.update_ip_addrs(|addrs| {
                    let mut i = 0;
                    while i < addrs.len() {
                        if matches!(addrs[i].address(), IpAddress::Ipv4(_)) {
                            addrs.remove(i);
                        } else {
                            i += 1;
                        }
                    }
                    if addrs.push(IpCidr::Ipv4(info.address)).is_err() {
                        println!("[NETWORK-SERVICE] ERROR: addr list full, DHCP IP not applied");
                    }
                });
            }
            Some(smoltcp::socket::dhcpv4::Event::Deconfigured) => {
                *current_dhcp_config = None;
                if dhcp_deconfigured_since.is_none() {
                    *dhcp_deconfigured_since = Some(uptime_ms);
                }
                println!("[NETWORK-SERVICE] eth0 DHCP deconfigured (lease expired or no response)");
                iface.routes_mut().remove_default_ipv4_route();
                iface.update_ip_addrs(|addrs| {
                    let mut i = 0;
                    while i < addrs.len() {
                        if matches!(addrs[i].address(), IpAddress::Ipv4(_)) {
                            addrs.remove(i);
                        } else {
                            i += 1;
                        }
                    }
                });
            }
        }
    }

    // 5. DHCP Watchdog: si link UP y sin IPv4 > 12s, forzar reset
    if use_dhcp {
        if let Some(since) = dhcp_deconfigured_since {
            if link_up && uptime_ms - *since > 12000 {
                println!("[NETWORK-SERVICE] DHCP Watchdog: No IPv4 for 12s on active link. Forcing DHCP reset...");
                sockets.get_mut::<smoltcp::socket::dhcpv4::Socket>(dhcp_handle).reset();
                *dhcp_deconfigured_since = Some(uptime_ms);
            }
        }
    }
}

fn get_extended_stats(
    iface: &Interface,
    sockets: &SocketSet,
    dns_handle: smoltcp::iface::SocketHandle,
    rx_total: u64,
    tx_total: u64,
    link_up: bool,
    static_dns_v4: Option<[u8; 4]>,
    static_gateway_v4: Option<[u8; 4]>,
) -> NetExtendedStats {
    let mut stats = NetExtendedStats {
        lo_ipv4: [127, 0, 0, 1],
        lo_ipv4_prefix: 8,
        lo_ipv6: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
        lo_ipv6_prefix: 128,
        lo_up: 1,
        eth0_ipv4: [0; 4],
        eth0_ipv4_prefix: 0,
        eth0_ipv6: [0; 16],
        eth0_ipv6_prefix: 0,
        eth0_up: if link_up { 1 } else { 0 },
        eth0_gateway: [0; 4],
        eth0_gateway_ipv6: [0; 16],
        eth0_dns: [0; 4],
        eth0_dns_ipv6: [0; 16],
        rx_bytes: rx_total,
        tx_bytes: tx_total,
    };

    for addr in iface.ip_addrs() {
        match addr.address() {
            IpAddress::Ipv4(a) => {
                stats.eth0_ipv4 = a.octets();
                stats.eth0_ipv4_prefix = addr.prefix_len();
            }
            IpAddress::Ipv6(a) => {
                stats.eth0_ipv6 = a.octets();
                stats.eth0_ipv6_prefix = addr.prefix_len();
            }
        }
    }

    if let Some(gw) = static_gateway_v4 {
        stats.eth0_gateway = gw;
    }

    let dns_socket = sockets.get::<DnsSocket>(dns_handle);
    // In smoltcp 0.13.0, servers is a private field. We use our tracked static_dns_v4.
    if let Some(dns) = static_dns_v4 {
        stats.eth0_dns = dns;
    }

    if stats.eth0_dns == [0; 4] {
        stats.eth0_dns = [8, 8, 8, 8];
    }

    stats
}

fn main() {
    let pid = unsafe { getpid() };
    println!("+--------------------------------------------------------------+");
    println!("|              NETWORK SERVICE (SMOLTCP)                       |");
    println!("+--------------------------------------------------------------+");
    println!("[NETWORK-SERVICE] Starting (PID: {})", pid);

    // 1. Open the raw ethernet device
    let mut device = match RawEthernetDevice::new(0) {
        Some(d) => {
            println!("[NETWORK-SERVICE] Connected to eth:0 (MAC: {})", d.mac);
            d
        }
        None => {
            println!("[NETWORK-SERVICE] ERROR: Could not open eth:0. Offline mode.");
            // Still handle IPC so the compositor doesn't wait forever for stats.
            let mut offline_buf = [0u8; 1024];
            loop {
                let (len, sender_pid) = receive_ipc(&mut offline_buf);
                if len > 0 {
                    let msg = &offline_buf[..len];
                    if msg.starts_with(b"GET_NET_EXT_STATS") {
                        let stats = NetExtendedStats {
                            lo_ipv4: [127, 0, 0, 1],
                            lo_ipv4_prefix: 8,
                            lo_ipv6: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
                            lo_ipv6_prefix: 128,
                            lo_up: 1,
                            eth0_ipv4: [0; 4],
                            eth0_ipv4_prefix: 0,
                            eth0_ipv6: [0; 16],
                            eth0_ipv6_prefix: 0,
                            eth0_up: 0,
                            eth0_gateway: [0; 4],
                            eth0_gateway_ipv6: [0; 16],
                            eth0_dns: [0; 4],
                            eth0_dns_ipv6: [0; 16],
                            rx_bytes: 0,
                            tx_bytes: 0,
                        };
                        let mut resp = [0u8; 4 + core::mem::size_of::<NetExtendedStats>()];
                        resp[0..4].copy_from_slice(b"NEXS");
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                &stats as *const _ as *const u8,
                                resp.as_mut_ptr().add(4),
                                core::mem::size_of::<NetExtendedStats>(),
                            );
                        }
                        send_ipc(sender_pid, 0x08, &resp);
                    } else if msg.starts_with(b"GET_NET_STATS") {
                        let mut resp = [0u8; 20];
                        resp[0..4].copy_from_slice(b"NSTA");
                        // rx and tx are both zero in offline mode
                        send_ipc(sender_pid, 0x08, &resp);
                    }
                }
                unsafe { sleep_ms(10); }
            }
        }
    };

    // 2. Initialize the stack
    println!("[NETWORK-SERVICE] Initializing interfaces...");

    // 2a. Loopback Interface
    let lo_config = Config::new(EthernetAddress([0, 0, 0, 0, 0, 0]).into());
    let mut lo_device = Loopback::new(Medium::Ethernet);
    let mut lo_iface = Interface::new(lo_config, &mut lo_device, Instant::from_millis(get_now_ms() as i64));
    lo_iface.update_ip_addrs(|addrs| {
        addrs.push(IpCidr::new(Ipv4Address::new(127, 0, 0, 1).into(), 8)).ok();
        addrs.push(IpCidr::new(Ipv6Address::new(0, 0, 0, 0, 0, 0, 0, 1).into(), 128)).ok();
    });
    let mut lo_sockets = SocketSet::new(vec![]);

    // 2b. Ethernet Interface (eth0)
    let config = Config::new(device.mac.into());
    let mut iface = Interface::new(config, &mut device, Instant::from_millis(get_now_ms() as i64));
    
    // Add Link-Local IPv6 address
    iface.update_ip_addrs(|addrs| {
        let mut ll_bytes = [0u8; 16];
        ll_bytes[0] = 0xfe;
        ll_bytes[1] = 0x80;
        let mac = device.mac.0;
        ll_bytes[8] = mac[0] ^ 0x02;
        ll_bytes[9] = mac[1];
        ll_bytes[10] = mac[2];
        ll_bytes[11] = 0xff;
        ll_bytes[12] = 0xfe;
        ll_bytes[13] = mac[3];
        ll_bytes[14] = mac[4];
        ll_bytes[15] = mac[5];
        let ll_octets: [u8; 16] = ll_bytes.try_into().unwrap();
        addrs.push(IpCidr::new(Ipv6Address::from_octets(ll_octets).into(), 64)).ok();
    });
    // Map kernel resource_id to smoltcp socket handles
    let mut kernel_sockets: BTreeMap<u64, smoltcp::iface::SocketHandle> = BTreeMap::new();
    let mut sockets = SocketSet::new(vec![]);
    
    // Initialize DHCPv4 socket for eth0
    let dhcp_socket = smoltcp::socket::dhcpv4::Socket::new();
    let dhcp_handle = sockets.add(dhcp_socket);

    // Initialize DNS socket
    let dns_socket = smoltcp::socket::dns::Socket::new(
        &[IpAddress::Ipv4(Ipv4Address::new(8, 8, 8, 8))], 
        vec![]
    );
    let dns_handle = sockets.add(dns_socket);

    let mut next_resource_id = 1u64;
    let mut current_dhcp_config: Option<DhcpInfo> = None;
    let mut use_dhcp = true;
    let mut static_dns_v4: Option<[u8; 4]> = None;
    let mut static_gateway_v4: Option<[u8; 4]> = None;

    println!("[NETWORK-SERVICE] TCP/IP stack initialized:");
    println!("  lo:   127.0.0.1, ::1");
    if let Some(addr) = iface.ip_addrs().iter().find(|a| matches!(a.address(), IpAddress::Ipv6(_))) {
        println!("  eth0: Link-Local IPv6: {}", addr.address());
    }

    // 3. Super-bucle principal: network_task + IPC + sleep
    let mut ipc_buf = [0u8; 1024];
    let mut resp_buf = [0u8; 1024];
    let mut last_activity_log = get_now_ms();
    // Arm the DHCP watchdog from service start so it fires even if DHCP was
    // never configured (e.g. when the physical link comes up after boot).
    let mut dhcp_deconfigured_since: Option<u64> = Some(get_now_ms());
    // Track the previous link state so we can detect UP transitions and
    // immediately trigger a DHCP re-discovery instead of waiting for smoltcp's
    // exponential backoff timer.
    let mut prev_link_up = device.get_link_status();

    loop {
        // Ejecutar la tarea de red (poll + eventos DHCP)
        network_task(
            &mut device,
            &mut iface,
            &mut sockets,
            &mut lo_iface,
            &mut lo_device,
            &mut lo_sockets,
            dhcp_handle,
            &mut current_dhcp_config,
            &mut dhcp_deconfigured_since,
            &mut prev_link_up,
            use_dhcp,
        );

        let rx_total = device.rx_bytes.load(Ordering::Relaxed);
        let tx_total = device.tx_bytes.load(Ordering::Relaxed);
        let link_up = device.get_link_status();

        // Activity logging (every 10 seconds)
        if get_now_ms() - last_activity_log > 10000 {
            println!("[NETWORK-SERVICE] Stats: RX={} TX={} Link={} Mode={}", 
                rx_total, tx_total, 
                if link_up { "UP" } else { "DOWN" },
                if use_dhcp { if current_dhcp_config.is_some() { "BOUND" } else { "SEARCHING" } } else { "STATIC" }
            );
            last_activity_log = get_now_ms();
        }

        // 4. Handle IPC messages for socket syscalls
        let (len, sender_pid) = receive_ipc(&mut ipc_buf);
        let mut processed = false;
        if len >= core::mem::size_of::<NetRequestHeader>() {
            let header = unsafe { &*(ipc_buf.as_ptr() as *const NetRequestHeader) };
            if header.magic == *NET_MAGIC {
                processed = true;
                let mut status: i64 = -1; // Default error
                let mut resp_data_size: u32 = 0;
                if sender_pid == 0x01 {
                    match header.op {
                        NetOp::Socket => {
                            let path_ptr = unsafe { ipc_buf.as_ptr().add(core::mem::size_of::<NetRequestHeader>()) };
                            let path_len = len.saturating_sub(core::mem::size_of::<NetRequestHeader>());
                            let path = unsafe { core::str::from_utf8(core::slice::from_raw_parts(path_ptr, path_len)).unwrap_or("") };
                            
                            let mut parts = path.split('/');
                            let _domain = parts.next();
                            let type_str = parts.next().unwrap_or("1");
                            
                            if type_str == "1" { // SOCK_STREAM
                                let tcp_rx_buffer = smoltcp::socket::tcp::SocketBuffer::new(vec![0; 4096]);
                                let tcp_tx_buffer = smoltcp::socket::tcp::SocketBuffer::new(vec![0; 4096]);
                                let tcp_socket = smoltcp::socket::tcp::Socket::new(tcp_rx_buffer, tcp_tx_buffer);
                                let handle = sockets.add(tcp_socket);
                                let id = next_resource_id;
                                next_resource_id += 1;
                                kernel_sockets.insert(id, handle);
                                status = id as i64;
                            } else if type_str == "2" { // SOCK_DGRAM
                                let udp_rx_buffer = smoltcp::socket::udp::PacketBuffer::new(vec![smoltcp::socket::udp::PacketMetadata::EMPTY; 16], vec![0; 4096]);
                                let udp_tx_buffer = smoltcp::socket::udp::PacketBuffer::new(vec![smoltcp::socket::udp::PacketMetadata::EMPTY; 16], vec![0; 4096]);
                                let udp_socket = smoltcp::socket::udp::Socket::new(udp_rx_buffer, udp_tx_buffer);
                                let handle = sockets.add(udp_socket);
                                let id = next_resource_id;
                                next_resource_id += 1;
                                kernel_sockets.insert(id, handle);
                                status = id as i64;
                            }
                        }
                        NetOp::Bind => {
                            let payload = unsafe { ipc_buf.as_ptr().add(core::mem::size_of::<NetRequestHeader>()) };
                            let plen = len.saturating_sub(core::mem::size_of::<NetRequestHeader>());
                            let path = unsafe { core::str::from_utf8(core::slice::from_raw_parts(payload, plen)).unwrap_or("") };
                            
                            if let Some((ip_str, port_str)) = path.split_once(':') {
                                if let (Ok(_ip), Ok(_port)) = (ip_str.parse::<smoltcp::wire::Ipv4Address>(), port_str.parse::<u16>()) {
                                    status = 0;
                                }
                            }
                        }
                        NetOp::Listen => {
                            if let Some(&handle) = kernel_sockets.get(&header.resource_id) {
                                let socket = sockets.get_mut::<smoltcp::socket::tcp::Socket>(handle);
                                if let Ok(_) = socket.listen(80) { // Default to 80 for PoC
                                    status = 0;
                                }
                            }
                        }
                        NetOp::Accept => {
                            if let Some(&handle) = kernel_sockets.get(&header.resource_id) {
                                let socket = sockets.get_mut::<smoltcp::socket::tcp::Socket>(handle);
                                if socket.is_active() && socket.state() == smoltcp::socket::tcp::State::Established {
                                    status = header.resource_id as i64; 
                                } else {
                                    status = -(scheme_error::EAGAIN as i64);
                                }
                            }
                        }
                        NetOp::Connect => {
                            if let Some(&handle) = kernel_sockets.get(&header.resource_id) {
                                let socket = sockets.get_mut::<smoltcp::socket::tcp::Socket>(handle);
                                let payload = unsafe { ipc_buf.as_ptr().add(core::mem::size_of::<NetRequestHeader>()) };
                                let mut ip_bytes = [0u8; 4];
                                unsafe { core::ptr::copy_nonoverlapping(payload, ip_bytes.as_mut_ptr(), 4); }
                                let port = unsafe { u16::from_be(core::ptr::read_unaligned(payload.add(4) as *const u16)) };
                                
                                let remote_addr = smoltcp::wire::Ipv4Address::from_octets(ip_bytes);
                                if let Ok(_) = socket.connect(iface.context(), (remote_addr, port), (smoltcp::wire::Ipv4Address::new(0, 0, 0, 0), 0)) {
                                    status = 0;
                                }
                            }
                        }
                        NetOp::Send => {
                            if let Some(&handle) = kernel_sockets.get(&header.resource_id) {
                                let payload = unsafe { ipc_buf.as_ptr().add(core::mem::size_of::<NetRequestHeader>()) };
                                let plen = len.saturating_sub(core::mem::size_of::<NetRequestHeader>());
                                let socket = sockets.get_mut::<smoltcp::socket::tcp::Socket>(handle);
                                if socket.can_send() {
                                    if let Ok(n) = socket.send_slice(unsafe { core::slice::from_raw_parts(payload, plen) }) {
                                        status = n as i64;
                                    }
                                }
                            }
                        }
                        NetOp::Recv => {
                            if let Some(&handle) = kernel_sockets.get(&header.resource_id) {
                                let socket = sockets.get_mut::<smoltcp::socket::tcp::Socket>(handle);
                                if socket.can_recv() {
                                    let resp_payload = unsafe { resp_buf.as_mut_ptr().add(core::mem::size_of::<NetResponseHeader>()) };
                                    if let Ok(n) = socket.recv_slice(unsafe { core::slice::from_raw_parts_mut(resp_payload, 500) }) {
                                        status = n as i64;
                                        resp_data_size = n as u32;
                                    }
                                } else {
                                    status = 0;
                                }
                            }
                        }
                        NetOp::Resolve => {
                            let payload = unsafe { ipc_buf.as_ptr().add(core::mem::size_of::<NetRequestHeader>()) };
                            let plen = len.saturating_sub(core::mem::size_of::<NetRequestHeader>());
                            let hostname = unsafe { core::str::from_utf8(core::slice::from_raw_parts(payload, plen)).unwrap_or("") };
                            
                            let socket = sockets.get_mut::<DnsSocket>(dns_handle);
                            if let Ok(handle) = socket.start_query(iface.context(), hostname, DnsQueryType::A) {
                                // Simple blocking wait for PoC (max 5 seconds)
                                let start_wait = get_now_ms();
                                while get_now_ms() - start_wait < 5000 {
                                    let now = Instant::from_millis(get_now_ms() as i64);
                                    iface.poll(now, &mut device, &mut sockets);
                                    
                                    let socket = sockets.get_mut::<DnsSocket>(dns_handle);
                                    match socket.get_query_result(handle) {
                                        Ok(addrs) => {
                                            if let Some(ip) = addrs.first() {
                                                let resp_payload = unsafe { resp_buf.as_mut_ptr().add(core::mem::size_of::<NetResponseHeader>()) };
                                                let copy_len = match ip {
                                                    IpAddress::Ipv4(a) => {
                                                        let o = a.octets();
                                                        unsafe { core::ptr::copy_nonoverlapping(o.as_ptr(), resp_payload, 4); }
                                                        4
                                                    }
                                                    IpAddress::Ipv6(a) => {
                                                        let o = a.octets();
                                                        unsafe { core::ptr::copy_nonoverlapping(o.as_ptr(), resp_payload, 16); }
                                                        16
                                                    }
                                                };
                                                status = 0;
                                                resp_data_size = copy_len as u32;
                                                break;
                                            }
                                        }
                                        Err(smoltcp::socket::dns::GetQueryResultError::Pending) => {
                                            unsafe { sleep_ms(10); }
                                        }
                                        Err(_) => {
                                            status = -1;
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                        NetOp::GetExtendedStats => {
                            let stats = get_extended_stats(&iface, &sockets, dns_handle, rx_total, tx_total, link_up, static_dns_v4, static_gateway_v4);

                            if stats.eth0_ipv4 != [0; 4] {
                                println!("[NETWORK-SERVICE] GET_NET_EXT_STATS (Syscall): Reporting Ipv4: {}.{}.{}.{}/{}", 
                                    stats.eth0_ipv4[0], stats.eth0_ipv4[1], stats.eth0_ipv4[2], stats.eth0_ipv4[3], stats.eth0_ipv4_prefix);
                            } else {
                                println!("[NETWORK-SERVICE] GET_NET_EXT_STATS (Syscall): No IPv4 found on eth0!");
                            }

                            let resp_payload = unsafe { resp_buf.as_mut_ptr().add(core::mem::size_of::<NetResponseHeader>()) };
                            unsafe { core::ptr::copy_nonoverlapping(&stats as *const _ as *const u8, resp_payload, core::mem::size_of::<NetExtendedStats>()); }
                            status = 0;
                            resp_data_size = core::mem::size_of::<NetExtendedStats>() as u32;
                        }
                        _ => {}
                    }
                }

                // Global / Non-restricted Ops (can come from compositor or other authorized services)
                if status == -1 {
                    match header.op {
                        NetOp::SetStaticConfig => {
                            let payload = unsafe { ipc_buf.as_ptr().add(core::mem::size_of::<NetRequestHeader>()) };
                            let config = unsafe { &*(payload as *const NetStaticConfig) };
                            
                            println!("[NETWORK-SERVICE] SetStaticConfig received");
                            use_dhcp = false;
                            current_dhcp_config = None;
                            
                            // Apply IPv4
                            iface.update_ip_addrs(|addrs| {
                                // Keep IPv6 Link-Local but clear others
                                addrs.retain(|a| {
                                    if let IpAddress::Ipv6(a6) = a.address() {
                                        a6.is_unicast_link_local()
                                    } else {
                                        false
                                    }
                                });
                                let ipv4 = Ipv4Address::from_octets(config.ipv4);
                                if !ipv4.is_unspecified() {
                                    let _ = addrs.push(IpCidr::new(ipv4.into(), config.ipv4_prefix));
                                }
                            });
                            
                            // Apply IPv6
                            if config.ipv6 != [0; 16] {
                                iface.update_ip_addrs(|addrs| {
                                    let ipv6 = Ipv6Address::from_octets(config.ipv6);
                                    let _ = addrs.push(IpCidr::new(ipv6.into(), config.ipv6_prefix));
                                });
                            }
                            
                            // Apply Gateway
                            iface.routes_mut().remove_default_ipv4_route();
                            let gw_v4 = Ipv4Address::from_octets(config.gateway_v4);
                            if !gw_v4.is_unspecified() {
                                let _ = iface.routes_mut().add_default_ipv4_route(gw_v4);
                                static_gateway_v4 = Some(config.gateway_v4);
                            }
                            
                            // DNS update
                            let dns_v4_val = Ipv4Address::from_octets(config.dns_v4);
                            if !dns_v4_val.is_unspecified() {
                                let dns_socket = sockets.get_mut::<DnsSocket>(dns_handle);
                                dns_socket.update_servers(&[IpAddress::Ipv4(dns_v4_val)]);
                                static_dns_v4 = Some(config.dns_v4);
                            }
                            
                            status = 0;
                        }
                        NetOp::SetDhcpConfig => {
                            println!("[NETWORK-SERVICE] SetDhcpConfig received from PID {}", sender_pid);
                            use_dhcp = true;
                            current_dhcp_config = None;
                            sockets.get_mut::<smoltcp::socket::dhcpv4::Socket>(dhcp_handle).reset();
                            iface.routes_mut().remove_default_ipv4_route();
                            iface.update_ip_addrs(|addrs| {
                                // Keep IPv6 Link-Local but clear others
                                addrs.retain(|a| {
                                    if let IpAddress::Ipv6(a6) = a.address() {
                                        a6.is_unicast_link_local()
                                    } else {
                                        false
                                    }
                                });
                            });
                            status = 0;
                        }
                        NetOp::Close => {
                            if let Some(handle) = kernel_sockets.remove(&header.resource_id) {
                                sockets.remove(handle);
                                status = 0;
                            }
                        }
                        _ => {}
                    }
                }

                    // Send response
                    let resp_header = NetResponseHeader {
                        magic: *NET_MAGIC,
                        op: NetOp::Response,
                        request_id: header.request_id,
                        status,
                        data_size: resp_data_size,
                    };
                    unsafe {
                        core::ptr::copy_nonoverlapping(&resp_header as *const _ as *const u8, resp_buf.as_mut_ptr(), core::mem::size_of::<NetResponseHeader>());
                    }
                    let total_resp_size = core::mem::size_of::<NetResponseHeader>() + resp_data_size as usize;
                    send_ipc(sender_pid, 0x08, &resp_buf[..total_resp_size]);
                }
        }

        if !processed && len > 0 {
            let msg_data = &ipc_buf[..len];
            if msg_data.starts_with(b"GET_NET_EXT_STATS") {
                let stats = get_extended_stats(&iface, &sockets, dns_handle, rx_total, tx_total, link_up, static_dns_v4, static_gateway_v4);
                
                if stats.eth0_ipv4 != [0; 4] {
                    println!("[NETWORK-SERVICE] GET_NET_EXT_STATS (IPC): Reporting Ipv4: {}.{}.{}.{}/{}", 
                        stats.eth0_ipv4[0], stats.eth0_ipv4[1], stats.eth0_ipv4[2], stats.eth0_ipv4[3], stats.eth0_ipv4_prefix);
                } else {
                    println!("[NETWORK-SERVICE] GET_NET_EXT_STATS (IPC): No IPv4 found!");
                }

                let mut resp = [0u8; 4 + core::mem::size_of::<NetExtendedStats>()];
                resp[0..4].copy_from_slice(b"NEXS");
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        &stats as *const _ as *const u8,
                        resp.as_mut_ptr().add(4),
                        core::mem::size_of::<NetExtendedStats>()
                    );
                }
                send_ipc(sender_pid, 0x08, &resp);
            } else if msg_data.starts_with(b"RENEW_DHCP") {
                println!("[NETWORK-SERVICE] RENEW_DHCP requested by PID {}", sender_pid);
                sockets.get_mut::<smoltcp::socket::dhcpv4::Socket>(dhcp_handle).reset();
                iface.routes_mut().remove_default_ipv4_route();
                iface.update_ip_addrs(|addrs| {
                    // Keep IPv6 Link-Local but clear IPv4
                    addrs.retain(|a| matches!(a.address(), smoltcp::wire::IpAddress::Ipv6(_)));
                });
                current_dhcp_config = None;
                
                let mut resp = [0u8; 4];
                resp.copy_from_slice(b"OK  ");
                send_ipc(sender_pid, 0x08, &resp);
            }
        }

        // Gestión de CPU: ceder tiempo en lugar de busy-wait al 100%
        unsafe { sleep_ms(1); }
    }
}
