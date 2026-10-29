//! La secuencia de arranque entera, en el orden de `xgbe_init`.
//!
//! Las tandas 10 a 28 dejaron cada pieza escrita y probada por separado.
//! Esto las encadena en el orden que Linux usa, que es la otra mitad del
//! problema: casi todas se pueden escribir en cualquier orden y unas
//! pocas no.
//!
//! Nada de esto ha corrido en hardware: QEMU no emula un XGMAC. Va detras
//! de la escotilla `xgbe.enable`, apagada por defecto.

use super::*;
use core::sync::atomic::{AtomicBool, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Called from `zCore`'s cmdline parsing when `xgbe.enable` is present.
/// Default off: nothing here has run against the part.
pub fn set_enabled(v: bool) {
    ENABLED.store(v, Ordering::Relaxed);
}

/// Is the hatch open?
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Everything the sequence needs that it cannot read off the chip.
#[derive(Debug, Clone, Copy)]
pub struct BringUpConfig {
    /// Which generation this is, for the handful of version-dependent steps.
    pub variant: Variant,
    /// `SNPSVER` from `MAC_VR`.
    pub snps_ver: u8,
    /// How many DMA channels to use each way.
    pub tx_channels: u32,
    /// Receive channels.
    pub rx_channels: u32,
    /// How many MTL queues each way.
    pub tx_queues: u32,
    /// Receive queues.
    pub rx_queues: u32,
    /// Traffic classes the part reports.
    pub tc_count: u32,
    /// The FIFO size each way, in bytes, already clamped by the variant.
    pub tx_fifo: u32,
    /// Receive FIFO.
    pub rx_fifo: u32,
    /// The MTU to configure for.
    pub mtu: u32,
    /// The station address.
    pub mac: [u8; 6],
}

/// The whole of `xgbe_init`, in Linux's order, plus the ring memory.
///
/// Three orderings in here are load-bearing and the rest are not:
///
/// * the **flush of the transmit queues comes first**, before anything else
///   is configured, because a queue still holding a frame from a previous
///   life will keep holding it across every register write below;
/// * the **FIFO split comes before the thresholds and the flow control**,
///   because both of those are expressed in units of a queue's size;
/// * and the **MAC enables come last**, after the filters and the length
///   limits, so there is no window where the receiver is on and the frame
///   trimming is not — the same rule that [`enable_rx`] already follows
///   inside itself.
///
/// Everything else is written because Linux writes it, in the order Linux
/// writes it, so that a divergence shows up as a diff rather than as a
/// behaviour nobody can place.
pub fn bring_up(
    acc: &dyn MmioAccess,
    cfg: &BringUpConfig,
    wait: &mut dyn FnMut(),
) -> DeviceResult<XgbeRings> {
    // Flush whatever a previous owner left in the transmit queues.
    flush_tx_queues(acc, cfg.snps_ver, cfg.tx_queues, wait)?;

    // The DMA side.
    config_dma_cache(acc, DMA_PCI_ARCR, DMA_PCI_AWCR, DMA_PCI_AWARCR);
    config_osp_mode(acc, cfg.tx_channels, false);
    config_pbl_val(acc, cfg.tx_channels, cfg.rx_channels, 32)?;
    config_rx_coalesce(acc, cfg.rx_channels, 0);
    config_tx_coalesce(acc);
    let mut rings = XgbeRings::alloc(0, cfg.mtu)?;
    config_rx_buffer_size(acc, cfg.rx_channels, rings.buffer_size())?;
    config_tso_mode(acc, cfg.tx_channels);
    rings.program(acc)?;
    enable_channel_interrupts(acc, 0, cfg.snps_ver, true, true);

    // The MTL side. The FIFO split has to come before anything measured in
    // units of a queue's size.
    config_mtl_scheduling(acc);
    config_tc_weights(acc, cfg.tc_count, cfg.tx_queues)?;
    map_tx_queues_to_tc(acc, cfg.tx_queues, cfg.tc_count)?;
    map_prios_to_rx_queues(acc, cfg.rx_queues)?;
    map_rx_queues_to_channels(acc, cfg.rx_queues)?;
    config_tx_forwarding(acc, cfg.tx_queues, Forwarding::StoreAndForward);
    config_rx_forwarding(acc, cfg.rx_queues, Forwarding::StoreAndForward);
    config_tx_fifo(acc, cfg.tx_fifo, cfg.tx_queues)?;
    config_rx_fifo(acc, cfg.rx_fifo, cfg.rx_queues)?;

    // The MAC side.
    set_mac_address(acc, cfg.mac);
    config_rx_filter(
        acc,
        &RxFilter {
            promiscuous: false,
            all_multicast: false,
            hash_unicast: false,
            hash_multicast: false,
        },
    );
    config_jumbo_enable(acc, cfg.mtu);
    // The water marks are per queue and are expressed in units of that
    // queue's own share of the FIFO, which is why this comes after the split.
    //
    // And the share has to be in BYTES here. `equal_fifo_share` returns the
    // REGISTER value, which is units of 256 bytes minus one, so passing that
    // in would ask for water marks against a queue about a thousand times
    // smaller than the real one --- and the result would still be in range,
    // which is the kind of wrong that does not announce itself.
    if cfg.rx_queues == 0 {
        return Err(DeviceError::InvalidParam);
    }
    let share_bytes = cfg.rx_fifo / cfg.rx_queues;
    let fc = queue_flow_control(share_bytes, MAX_JUMBO_PACKET);
    for queue in 0..cfg.rx_queues {
        config_flow_control(acc, queue, &fc);
    }
    set_mac_speed(acc, LinkSpeed::Speed10G, cfg.snps_ver);
    config_rx_csum(acc, true);
    config_vlan_support(acc, &[], false, false)?;
    config_mmc(acc);
    enable_mac_interrupts(acc);

    // And only now the enables.
    enable_tx(acc, cfg.tx_channels, cfg.tx_queues);
    enable_rx(acc, cfg.rx_channels, cfg.rx_queues);

    Ok(rings)
}
