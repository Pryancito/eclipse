// Supports Realtek RTL8211F on Allwinner D1

use super::mii::*;
use super::utils::*;

use core::marker::PhantomData;
use core::mem::{size_of, size_of_val};

use super::Provider;
use super::{phys_to_virt, virt_to_phys};
use alloc::slice;
use alloc::sync::Arc;
use alloc::vec::Vec;

// RTL8211F
pub const GMAC_BASE: u32 = 0x04500000;
pub const CCU_BASE: u32 = 0x02001000;
pub const SYS_CFG_BASE: u32 = 0x03000000;
pub const PINCTRL_GPIO_BASE: u32 = 0x02000000;

const EMAC_BGR_REG: u32 = 0x097C; // CCU
const EMAC_25M_CLK_REG: u32 = 0x0970;

const EMAC_EPHY_CLK_REG0: u32 = 0x30; // SYS_CFG

// mac addr 3a:c5:31:d5:de:88
const MAC_ADDR: &str = "3a:c5:31:d5:de:88";

const DMA_DESC_RX: usize = 256;
const DMA_DESC_TX: usize = 256;
const BUDGET: usize = DMA_DESC_RX / 4;
const TX_THRESH: usize = DMA_DESC_TX / 4;

const MAX_BUF_SZ: u32 = 2048 - 1;

const TX_DELAY: u32 = 3;
const RX_DELAY: u32 = 0;

const MDC_CLOCK_RATIO: u32 = 0x03;

const GETH_BASIC_CTL0: u32 = 0x00;
const GETH_BASIC_CTL1: u32 = 0x04;
const GETH_INT_STA: u32 = 0x08;
const GETH_INT_EN: u32 = 0x0C;
const GETH_TX_CTL0: u32 = 0x10;
const GETH_TX_CTL1: u32 = 0x14;
const GETH_TX_FLOW_CTL: u32 = 0x1C;
const GETH_TX_DESC_LIST: u32 = 0x20;
const GETH_RX_CTL0: u32 = 0x24;
const GETH_RX_CTL1: u32 = 0x28;
const GETH_RX_DESC_LIST: u32 = 0x34;
const GETH_RX_FRM_FLT: u32 = 0x38;
const GETH_RX_HASH0: u32 = 0x40;
const GETH_RX_HASH1: u32 = 0x44;
const GETH_MDIO_ADDR: u32 = 0x48;
const GETH_MDIO_DATA: u32 = 0x4C;
const GETH_ADDR_HI: u32 = 0x50; //(0x50 + ((reg) << 3))
const GETH_ADDR_LO: u32 = 0x54; //(0x54 + ((reg) << 3))
const GETH_TX_DMA_STA: u32 = 0xB0;
const GETH_TX_CUR_DESC: u32 = 0xB4;
const GETH_TX_CUR_BUF: u32 = 0xB8;
const GETH_RX_DMA_STA: u32 = 0xC0;
const GETH_RX_CUR_DESC: u32 = 0xC4;
const GETH_RX_CUR_BUF: u32 = 0xC8;
const GETH_RGMII_STA: u32 = 0xD0;

const RGMII_IRQ: u32 = 0x00000001;

const MII: usize = 2;
const GMII: usize = 3;
const RMII: usize = 7;
const RGMII: usize = 8;

const CTL0_LM: u32 = 0x02;
const CTL0_DM: u32 = 0x01;
const CTL0_SPEED: u32 = 0x04;

const BURST_LEN: u32 = 0x3F000000;
const RX_TX_PRI: u32 = 0x02;
const SOFT_RST: u32 = 0x01;

const TX_FLUSH: u32 = 0x01;
const TX_MD: u32 = 0x02;
const TX_NEXT_FRM: u32 = 0x04;
const TX_TH: u32 = 0x0700;

const RX_FLUSH: u32 = 0x01;
const RX_MD: u32 = 0x02;
const RX_RUNT_FRM: u32 = 0x04;
const RX_ERR_FRM: u32 = 0x08;
const RX_TH: u32 = 0x0030;

const TX_INT: u32 = 0x00001;
const TX_STOP_INT: u32 = 0x00002;
const TX_UA_INT: u32 = 0x00004;
const TX_TOUT_INT: u32 = 0x00008;
const TX_UNF_INT: u32 = 0x00010;
const TX_EARLY_INT: u32 = 0x00020;
const RX_INT: u32 = 0x00100;
const RX_UA_INT: u32 = 0x00200;
const RX_STOP_INT: u32 = 0x00400;
const RX_TOUT_INT: u32 = 0x00800;
const RX_OVF_INT: u32 = 0x01000;
const RX_EARLY_INT: u32 = 0x02000;
const LINK_STA_INT: u32 = 0x10000;

const SPEED_10: i32 = 10;
const SPEED_100: i32 = 100;
const SPEED_1000: i32 = 1000;
const SPEED_UNKNOWN: i32 = -1;

const DUPLEX_HALF: i32 = 0x00;
const DUPLEX_FULL: i32 = 0x01;
const DUPLEX_UNKNOWN: i32 = 0xff;

const SF_DMA_MODE: usize = 1;

/* - 0: Flow Off
 * - 1: Rx Flow
 * - 2: Tx Flow
 * - 3: Rx & Tx Flow
 */
const FLOW_CTRL: u32 = 0;
const PAUSE: u32 = 0x400;

/* Enable or disable autonegotiation. */
const AUTONEG_DISABLE: usize = 0;
const AUTONEG_ENABLE: usize = 1;

/* Flow Control defines */
const FLOW_OFF: u32 = 0;
const FLOW_RX: u32 = 1;
const FLOW_TX: u32 = 2;
const FLOW_AUTO: u32 = FLOW_TX | FLOW_RX;
const HASH_TABLE_SIZE: u32 = 64;
const PAUSE_TIME: u32 = 0x200;
const GMAC_MAX_UNICAST_ADDRESSES: u32 = 8;

/* PHY address */
const PHY_ADDR: u32 = 0x01;
const PHY_DM: u32 = 0x0010;
const PHY_AUTO_NEG: u32 = 0x0020;
const PHY_POWERDOWN: u32 = 0x0080;
const PHY_NEG_EN: u32 = 0x1000;

const MII_BUSY: u32 = 0x00000001;
/// Upper bound on MDIO/PHY/reset busy-wait polls. The MDIO clock and PHY can be
/// misconfigured (or no cable present), in which case these registers never
/// clear; without a cap the kernel busy-spins forever and wedges boot. ~1e6
/// MMIO reads is far longer than any real completion yet still bounded.
const MDIO_MAX_SPINS: u32 = 1_000_000;
const MII_WRITE: u32 = 0x00000002;
const MII_PHY_MASK: u32 = 0x0000FFC0;
const MII_CR_MASK: u32 = 0x0000001C;
const MII_CLK: u32 = 0x00000008;

const MII_BMCR: u32 = 0x00;
const BMCR_RESET: u32 = 0x8000;
const BMCR_PDOWN: u32 = 0x0800;

#[derive(Debug, Copy, Clone)]
#[repr(C, packed)]
pub struct DmaDesc {
    // size: 16
    desc0: u32, // Status
    desc1: u32, // Buffer Size
    desc2: u32, // Buffer Addr
    desc3: u32, // Next Desc
}

pub enum RxFrameStatus {
    /* IPC status */
    GoodFrame = 0,
    DiscardFrame = 1,
    CsumNone = 2,
    LlcSnap = 4,
}

pub enum TxDmaIrqStatus {
    TxHardError = 1,
    TxHardErrorBumpTc = 2,
    HandleTxRx = 3,
}

pub struct RTL8211F<P: Provider> {
    base: u32,     // 0x4500000
    base_ccu: u32, // CCU_BASE
    base_phy: u32, // SYS_CFG

    pinctrl: u32, // 0x2000000

    mac: [u8; 6],
    recv_buffers: Vec<usize>,
    recv_ring: &'static mut [DmaDesc],

    send_buffers: Vec<usize>,
    send_ring: &'static mut [DmaDesc],

    phy_mode: usize,

    autoneg: usize,

    tx_delay: u32,
    rx_delay: u32,

    tx_dirty: usize,
    tx_clean: usize,
    rx_dirty: usize,
    rx_clean: usize,

    marker: PhantomData<P>,
}

impl<P> RTL8211F<P>
where
    P: Provider,
{
    pub fn new(mac_addr: &[u8; 6]) -> Self {
        assert_eq!(size_of::<DmaDesc>(), 16);

        let mut mac: [u8; 6] = [0; 6];
        let v_addr = mac_addr[0] as u32
            + mac_addr[1] as u32
            + mac_addr[2] as u32
            + mac_addr[3] as u32
            + mac_addr[4] as u32
            + mac_addr[5] as u32;
        // The broadcast arm (`v_addr == 0x5fa`, the sum of six 0xff bytes) can
        // never be the one that fires: 0x5fa is the largest sum six bytes can
        // reach, so it needs all six to be 0xff, and then bit 0 of byte 0 is set
        // and the multicast arm has already caught it. Kept because it says what
        // it is looking for.
        if (v_addr == 0) || // mac addr is all 0
           ((mac_addr[0] & 0x01) == 1) || // mac addr is multicast
           (v_addr == 0x5fa)
        // mac addr is broadcast
        {
            let tokens: Vec<&str> = MAC_ADDR.split(':').collect();
            for (i, s) in tokens.iter().enumerate() {
                mac[i] = u8::from_str_radix(s, 16).unwrap();
            }
        } else {
            mac = *mac_addr;
        }

        info!(
            "mac addr: {:x}:{:x}:{:x}:{:x}:{:x}:{:x}",
            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
        );

        // DMA使用的dma_desc内存，有一致性要求，一般非cache的
        // 而这里到时会flush_cache()来同步cache
        // dma_desc记得内存清零
        let (send_ring_va, send_ring_pa) = P::alloc_dma(P::PAGE_SIZE);
        let (recv_ring_va, recv_ring_pa) = P::alloc_dma(P::PAGE_SIZE);
        let send_ring = unsafe {
            slice::from_raw_parts_mut(
                send_ring_va as *mut DmaDesc,
                P::PAGE_SIZE / size_of::<DmaDesc>(), // 4096/16 = 256 个 dma_desc
            )
        };

        let recv_ring = unsafe {
            slice::from_raw_parts_mut(
                recv_ring_va as *mut DmaDesc,
                P::PAGE_SIZE / size_of::<DmaDesc>(),
            )
        };

        send_ring.fill(DmaDesc {
            desc0: 0,
            desc1: 0,
            desc2: 0,
            desc3: 0,
        });
        recv_ring.fill(DmaDesc {
            desc0: 0,
            desc1: 0,
            desc2: 0,
            desc3: 0,
        });

        let mut send_buffers = Vec::with_capacity(send_ring.len());
        let mut recv_buffers = Vec::with_capacity(recv_ring.len());

        info!("Set a ring desc buffer for TX");
        // Set a ring desc buffer for TX
        for i in 0..send_ring.len() {
            let (buffer_page_va, buffer_page_pa) = P::alloc_dma(P::PAGE_SIZE); // 其实buffer申请2K左右就可以

            // desc1.all |= (1 << 24) Chain mode
            send_ring[i].desc1 |= 1 << 24;

            send_ring[i].desc2 = buffer_page_pa as u32;

            if (i + 1) == send_ring.len() {
                send_ring[i].desc3 = virt_to_phys(&send_ring[0] as *const DmaDesc as usize) as u32;
            } else {
                send_ring[i].desc3 =
                    virt_to_phys(&send_ring[i + 1] as *const DmaDesc as usize) as u32;
            }

            send_buffers.push(buffer_page_va);
        }

        info!("Set a ring desc buffer for RX");
        // Set a ring desc buffer for RX
        for i in 0..recv_ring.len() {
            let (buffer_page_va, buffer_page_pa) = P::alloc_dma(P::PAGE_SIZE);

            recv_ring[i].desc1 |= 1 << 24;
            //recv_ring[i].desc2 = buffer_page_pa as u32;
            if (i + 1) == recv_ring.len() {
                recv_ring[i].desc3 = virt_to_phys(&recv_ring[0] as *const DmaDesc as usize) as u32;
            } else {
                recv_ring[i].desc3 =
                    virt_to_phys(&recv_ring[i + 1] as *const DmaDesc as usize) as u32;
            }

            recv_buffers.push(buffer_page_va);

            // geth_rx_refill, 实际运行refill时却是：priv->rx_clean: 0 ~ 254 ?
            // desc_buf_set(&mut recv_ring[i], buffer_page_pa as u32, MAX_BUF_SZ);
            recv_ring[i].desc1 &= !((1 << 11) - 1);
            recv_ring[i].desc1 |= MAX_BUF_SZ & ((1 << 11) - 1);
            recv_ring[i].desc2 = buffer_page_pa as u32;

            // sync memery, fence指令？

            desc_set_own(&mut recv_ring[i]);
        }

        info!(
            "send_buffers length: {}, recv_buffers length: {}",
            send_buffers.len(),
            recv_buffers.len()
        );

        RTL8211F {
            base: GMAC_BASE,
            base_ccu: CCU_BASE,
            base_phy: SYS_CFG_BASE,

            pinctrl: PINCTRL_GPIO_BASE,

            mac,
            recv_buffers,
            recv_ring,

            send_buffers,
            send_ring,

            phy_mode: RGMII,
            autoneg: AUTONEG_ENABLE,
            //autoneg: AUTONEG_DISABLE,
            tx_delay: TX_DELAY,
            rx_delay: RX_DELAY,

            tx_dirty: 0,
            tx_clean: 0,
            rx_dirty: 0,
            rx_clean: 0,

            marker: PhantomData,
        }
    }

    pub fn open(&mut self) -> Result<i32, &str> {
        // 初始化驱动之前设置 pinctrl
        self.pinctrl_gpio_set_gmac();

        // gmacirq 62 --> geth_interrupt()

        // ephy_clk CLK_EMAC0_25M

        if (self.phy_mode != MII) && (self.phy_mode != RGMII) && (self.phy_mode != RMII) {
            error!("Not support phy type !");
            self.phy_mode = MII;
        }

        self.power_on();
        self.clk_enable();

        // geth_phy_init

        /* If config gpio to reset the phy device, we should reset it */
        self.pinctrl_gpio_reset_gmac_phy();

        self.mdio_reset();

        // PHY_POLL = -1, linux驱动，如果不支持中断

        // #define PHY_MAX_ADDR 32
        let phyaddr = 0;
        self.mdio_write(phyaddr, MII_BMCR, BMCR_RESET);
        let mut reset_spins = 0u32;
        while (BMCR_RESET & self.mdio_read(phyaddr, MII_BMCR)) != 0 {
            //sleep(30);  // sleep 30 milliseconds
            reset_spins += 1;
            if reset_spins >= MDIO_MAX_SPINS {
                warn!("[rtl8211f] PHY BMCR reset did not clear; continuing");
                break;
            }
        }

        let mii_bmcr_value = self.mdio_read(phyaddr, MII_BMCR);
        self.mdio_write(phyaddr, MII_BMCR, mii_bmcr_value & !BMCR_PDOWN);
        info!("Read MII_BMCR: {:#x}", mii_bmcr_value);

        // `rtlx::init` already says, in a comment on the very call that lands
        // here, that a GMAC which does not come out of soft reset "must not take
        // the whole boot down" and that is why it propagates instead of
        // unwrapping. This `unwrap()` panicked before that error could ever
        // reach it, so the mitigation at the call site was dead: a board whose
        // EMAC clock or power is not up yet took the kernel with it instead of
        // booting without networking. Every other hardware wait in `open` is
        // bounded and carries on; this was the one that killed the machine.
        if self.mac_reset().is_err() {
            error!("[rtl8211f] GMAC soft reset never completed; no networking");
            return Err("mac Soft Reset failed !");
        }

        self.mac_init(1, 1);

        self.set_umac(&self.mac, 0);

        // dma_desc_init
        // Set a ring desc buffer
        // implemented in new()
        //
        self.rx_refill();

        flush_cache(
            virt_to_phys(&self.recv_ring[0] as *const DmaDesc as usize) as u64,
            size_of_val(self.recv_ring) as u64,
        );
        flush_cache(
            virt_to_phys(&self.send_ring[0] as *const DmaDesc as usize) as u64,
            size_of_val(self.send_ring) as u64,
        );

        // phy_start
        // 注意地址32位对齐
        self.start_rx(virt_to_phys(&self.recv_ring[0] as *const DmaDesc as usize) as u32);
        self.start_tx(virt_to_phys(&self.send_ring[0] as *const DmaDesc as usize) as u32);

        // Enable the Rx/Tx
        self.mac_enable();

        Ok(0)
    }

    pub fn pinctrl_gpio_set_gmac(&mut self) {
        // pctl->membase VA: 0xffffffd00405c000, name pinctrl@2000000 PA: 0x2000000

        // PE_CFG0
        self.pinctrl_gpio_set(0xc0, 0x88888888);

        // PE_PULL0, Pull_up/down disable, u-boot
        self.pinctrl_gpio_set(0xe4, 0x0);

        // PE_DRV0, multi driving select level0
        self.pinctrl_gpio_set(0xd4, 0x0);

        // PE_CFG1
        self.pinctrl_gpio_set(0xc4, 0x88888888);

        // PE_PULL0, Pull_up/down disable, u-boot
        self.pinctrl_gpio_set(0xe4, 0x0);

        // PE_DRV1, multi driving select level0
        self.pinctrl_gpio_set(0xd8, 0x0);
    }

    pub fn pinctrl_gpio_reset_gmac_phy(&mut self) {
        // set GPIO direction to output

        // PE Data Register
        // index 16
        self.pinctrl_gpio_set(0xd0, 0x0);

        // PE_CFG2, PE16 select Output
        self.pinctrl_gpio_set(0xc8, 0xf1);

        // sleep 50 milliseconds

        // PE Data Register
        // index 16
        self.pinctrl_gpio_set(0xd0, 0x10000); //第16位？

        // PE_CFG2, PE16 select Output
        self.pinctrl_gpio_set(0xc8, 0xf1);

        // sleep 50 milliseconds
    }

    pub fn set_rx_mode(&mut self) {
        self.set_filter(0x0);

        // Hash Multicast
        self.hash_filter(0x0, 0x1);
        self.set_filter(0x4);

        //self.adjust_link(); // 顺序不必需放这步

        // Pass all multicast
        self.hash_filter(0xffffffff, 0xffffffff);
        self.set_filter(0x10);

        // Promiscuous Mode
        self.set_filter(0x1);
    }

    pub fn adjust_link(&mut self) -> Result<i32, &str> {
        let phyaddr = 0;
        let mut link: u32 = 0;
        let mut autoneg_complete: u32 = 0;

        let mut duplex: i32 = DUPLEX_UNKNOWN;
        let mut speed: i32 = SPEED_UNKNOWN;
        let mut pause: i32 = 0;
        let mut asym_pause: i32 = 0;

        if self.autoneg == AUTONEG_ENABLE {
            // Auto negotiation

            // 在哪设置这__phy_modify_changed ?
            /*
            let setup = self.mdio_read(phyaddr, MII_BMCR);
            setup |= BMCR_SPEED1000;
            self.mdio_write(phyaddr, MII_BMCR, setup);
            */

            /* Setup standard advertisement */
            // The mask names every bit this write owns and `adv` is what it
            // leaves set, so a bit named only in the mask is a bit being
            // CLEARED. `ADVERTISE_PAUSE_CAP` and `ADVERTISE_PAUSE_ASYM` were in
            // the mask and missing from `adv`, so the driver told the link
            // partner it could not do 802.3x pause -- and then, sixty lines
            // down, asked the partner's reply whether pause had been agreed.
            // `lpa &= adv` makes that question answer "no" every time, which is
            // why `flow_ctrl` and the whole FLOW_* family below were dead code.
            // Advertise pause, which is what putting those bits in the mask was
            // reaching for. 100BASE4 stays cleared: this MAC cannot do it.
            let adv = ADVERTISE_ALL | ADVERTISE_PAUSE_CAP | ADVERTISE_PAUSE_ASYM;
            let _ = self.phy_modify(
                MII_ADVERTISE,
                ADVERTISE_ALL | ADVERTISE_100BASE4 | ADVERTISE_PAUSE_CAP | ADVERTISE_PAUSE_ASYM,
                adv,
            );

            // 1000M PHY BMSR_ESTATEN = 1
            let bmsr = self.mdio_read(phyaddr, MII_BMSR);
            let is_gigabit_capable = (bmsr & BMSR_ESTATEN) != 0;
            if is_gigabit_capable {
                let adv = ADVERTISE_1000FULL;
                let _ = self.phy_modify(MII_CTRL1000, ADVERTISE_1000FULL | ADVERTISE_1000HALF, adv);
            }

            self.phy_restart_aneg();

            // 在AUTONEG，autoneg_complete完成时，就开始解析设置自协商的单双工或速率等信息
            // read LPA todo
            // phy_resolve_aneg_linkmode() TODO 接着解析自协商的匹配速率

            // The extended-status bit read above IS the answer to "can this PHY
            // do gigabit", and it was being read and then ignored: this block
            // ran behind a hardcoded 1. On a PHY without gigabit, MII_STAT1000
            // and MII_CTRL1000 are reserved registers, so whatever they happen
            // to read back was parsed as a negotiation result -- and a reserved
            // register that reads all ones carries LPA_1000MSFAIL, which aborts
            // link-up with "Master/Slave resolution failed" on a perfectly good
            // 100M link.
            // 有Gigabit连接能力时
            if is_gigabit_capable {
                let lpagb = self.mdio_read(phyaddr, MII_STAT1000);
                info!("MII_STAT1000    : {:#x}", lpagb);

                let advgb = self.mdio_read(phyaddr, MII_CTRL1000);
                if (lpagb & LPA_1000MSFAIL) != 0 {
                    if (advgb & CTL1000_ENABLE_MASTER) != 0 {
                        error!(
                            "Master/Slave resolution failed, maybe conflicting manual settings ?"
                        );
                    } else {
                        error!("Master/Slave resolution failed");
                    }
                    return Err("Master/Slave resolution failed ! NOLINK");
                }

                // 这里更新1000M的信息, 等下接着更新100M/10M的信息
                //
                // MII_STAT1000寄存器获取对端的能力： LPA_1000FULL, LPA_1000HALF
                // 没有包括 Pause
                if ((lpagb & LPA_1000FULL) != 0) && ((advgb & ADVERTISE_1000FULL) != 0) {
                    speed = SPEED_1000;
                    duplex = DUPLEX_FULL;
                } else if ((lpagb & LPA_1000HALF) != 0) && ((advgb & ADVERTISE_1000HALF) != 0) {
                    speed = SPEED_1000;
                    duplex = DUPLEX_HALF;
                }
            }
            /////////
            //百兆以下

            let adv = self.mdio_read(phyaddr, MII_ADVERTISE);
            info!("MII_ADVERTISE   : {:#x}", adv);

            // MII_LPA寄存器获取对端的能力： 100/10M, Full/Half, Pause, Asym_Pause
            let mut lpa = self.mdio_read(phyaddr, MII_LPA);
            info!("MII_LPA         : {:#x}", lpa);

            lpa &= adv; // LINK能力，取你我交集
            info!("LPA & ADVERTISE : {:#x}", lpa);

            //speed按从高到低的优先顺序匹配
            if speed == SPEED_1000 {
            } else if (lpa & LPA_100FULL) != 0 {
                speed = SPEED_100;
                duplex = DUPLEX_FULL;
            } else if (lpa & LPA_100HALF) != 0 {
                speed = SPEED_100;
                duplex = DUPLEX_HALF;
            } else if (lpa & LPA_10FULL) != 0 {
                speed = SPEED_10;
                duplex = DUPLEX_FULL;
            } else {
                speed = SPEED_10;
                duplex = DUPLEX_HALF;
            }

            if duplex == DUPLEX_FULL {
                pause = if (lpa & LPA_PAUSE_CAP) != 0 { 1 } else { 0 };
                asym_pause = if (lpa & LPA_PAUSE_ASYM) != 0 { 1 } else { 0 };
            }

            // 解析到speed duplex并设置后,判断网线Link
        } else {
            // AUTONEG_DISABLE

            // Configures MII_BMCR to force speed/duplex
            //let ctl = BMCR_SPEED1000 | BMCR_FULLDPLX;
            // 默认设成 100/FULL
            let ctl = BMCR_SPEED100 | BMCR_FULLDPLX;
            let _ = self.phy_modify(MII_BMCR, !(BMCR_LOOPBACK | BMCR_ISOLATE | BMCR_PDOWN), ctl);
            // 开始check网卡link状态, 然后设置speed/duplex

            // genphy_read_status()
            let bmcr: u32 = self.mdio_read(phyaddr, MII_BMCR);
            info!("MII_BMCR: {:#x}", bmcr);
            if (bmcr & BMCR_FULLDPLX) != 0 {
                duplex = DUPLEX_FULL;
            } else {
                duplex = DUPLEX_HALF;
            }
            if (bmcr & BMCR_SPEED1000) != 0 {
                speed = SPEED_1000;
            } else if (bmcr & BMCR_SPEED100) != 0 {
                speed = SPEED_100;
            } else {
                speed = SPEED_10;
            }
            // AUTONEG_DISABLE
        }

        info!("DUPLEX: {}, SPEED: {}", duplex, speed);

        info!("Waiting for link ...");
        let mut link_spins = 0u32;
        loop {
            // Read link status
            let status = self.mdio_read(phyaddr, MII_BMSR);
            link = status & BMSR_LSTATUS;

            if link == BMSR_LSTATUS {
                info!("Link is up! status: {:#x}", status);
                break;
            }
            // Don't wedge boot forever when no cable / link partner is present.
            link_spins += 1;
            if link_spins >= MDIO_MAX_SPINS {
                warn!("[rtl8211f] no link (no cable?); continuing without link");
                break;
            }
        }

        // 而没网线Link时, 不进行下列设置: PHY state change UP -> NOLINK
        if link != 0 {
            // `FLOW_CTRL` is the hardwired "Flow Off" constant, so passing it
            // here enabled neither direction: `flow_ctrl` fell through both of
            // its `fc` arms and only wrote a pause TIME into a register whose
            // enable bit was still clear. What the negotiation has just worked
            // out is `pause`/`asym_pause`, so that is what decides.
            //
            // Symmetric pause agreed: honour the partner's pause frames and
            // send our own. Asymmetric only: we advertise PAUSE, so what the
            // partner asked for is that we honour its frames, not that we send
            // ours (IEEE 802.3 Annex 28B).
            let fc = if pause != 0 {
                FLOW_AUTO
            } else if asym_pause != 0 {
                FLOW_RX
            } else {
                FLOW_OFF
            };
            if fc != FLOW_OFF {
                // PAUSE_TIME is the quanta count the reference driver sends;
                // `PAUSE` is an advertisement bit that had been pressed into
                // service as a pause time, which is why PAUSE_TIME sat unused.
                self.flow_ctrl(duplex, fc, PAUSE_TIME);
            }

            self.set_link_mode(duplex, speed);
            // Link is Up
        }

        Ok(0)
        // 开始接收数据吧
    }

    pub fn can_recv(&mut self) -> bool {
        let desc = &self.recv_ring[self.rx_dirty];
        invalidate_dcache(
            virt_to_phys(desc as *const DmaDesc as usize) as u64,
            size_of::<DmaDesc>() as u64,
        );
        desc_get_own(desc) == 0
    }

    pub fn geth_recv(&mut self, limit: usize) -> (Vec<u8>, i32) {
        let mut rx_packets: u64 = 0;
        let mut rx_bytes: u64 = 0;
        let mut rxcount: usize = 0;
        let mut entry: usize = 0;
        let mut desc_count: usize = 0;
        let mut buffer: Vec<u8> = Vec::new();

        while rxcount < limit {
            entry = self.rx_dirty;
            let mut desc = &mut self.recv_ring[entry];

            invalidate_dcache(
                virt_to_phys(desc as *const DmaDesc as usize) as u64,
                size_of::<DmaDesc>() as u64,
            );

            if desc_get_own(desc) != 0 {
                break;
            }

            desc_count = entry;
            rxcount += 1;
            self.rx_dirty = (self.rx_dirty + 1) % DMA_DESC_RX;

            // Get length & status from hardware. Clamp to the DMA buffer size:
            // the hardware length field is 14 bits (up to 16383) and on the last
            // descriptor of a multi-descriptor frame it reports the WHOLE frame
            // length, which can exceed our per-buffer MAX_BUF_SZ. Using it
            // unclamped would read (and invalidate/flush cache) past the 1-page
            // RX buffer -> OOB read + corruption of adjacent kernel memory.
            let mut frame_len = ((desc.desc0 >> 16) & 0x3fff).min(MAX_BUF_SZ); // bit[16:29]

            //discard frame when last_desc, err_sum, len_err, mii_err
            let status = if (((desc.desc0 >> 8) & 0x1) == 0) || ((desc.desc0 & 0x9008) != 0) {
                RxFrameStatus::DiscardFrame as i32
            } else {
                RxFrameStatus::GoodFrame as i32
            };

            info!("RX frame size {}, status: {:?}", frame_len, status);

            if self.recv_buffers[entry] == 0 {
                error!("Recv buffer is NULL");
                break;
            }

            invalidate_dcache(
                virt_to_phys(self.recv_buffers[entry]) as u64,
                frame_len as u64,
            );

            let skb = unsafe {
                slice::from_raw_parts(self.recv_buffers[entry] as *const u8, frame_len as usize)
            };

            info!("========== RX PKT DATA: <<<<<<<<<<");
            print_hex_dump(skb, 64);

            if status == RxFrameStatus::DiscardFrame as i32 {
                debug!("Get error packet");

                // Just need to clear 64 bits header
                unsafe {
                    slice::from_raw_parts_mut(self.recv_buffers[entry] as *mut u8, 64).fill(0);
                }
                flush_cache(virt_to_phys(self.recv_buffers[entry]) as u64, 64);

                continue;
            }

            if status != RxFrameStatus::LlcSnap as i32 {
                // Guard the FCS strip: a runt/garbage frame shorter than the
                // 4-byte FCS would underflow frame_len (u32) to ~4 GiB and then
                // read/copy far out of bounds. Skip such frames.
                if frame_len < 4 {
                    continue;
                }
                frame_len -= 4; // ETH_FCS_LEN, 帧出错检验
            }

            flush_cache(
                virt_to_phys(self.recv_buffers[entry]) as u64,
                frame_len as u64,
            );

            //注意只接收一个网络帧, limit=1
            buffer = unsafe {
                slice::from_raw_parts(self.recv_buffers[entry] as *const u8, frame_len as usize)
                    .to_vec()
            };

            /*
            // skb_put(skb, frame_len);
            // dma_unmap_single
            P::dealloc_dma(self.recv_buffers[entry], P::PAGE_SIZE);
            self.recv_buffers[entry] = 0;
            */

            info!(
                "desc_buf_get_addr: {:#x}, desc_buf_get_len: {}",
                desc.desc2,
                desc.desc1 & ((1 << 11) - 1)
            );

            //u-boot testing
            /*
            let paddr = desc.desc2 as u32;
            desc_buf_set(desc, paddr, MAX_BUF_SZ);
            desc_set_own(desc);
            */

            // eth_type_trans 包的协议分析

            rx_packets += 1;
            rx_bytes += frame_len as u64;

            // One frame in, one frame out. The loop used to keep going and
            // overwrite `buffer` on every pass, so with any limit above 1 it
            // took frames out of the ring and returned only the last one:
            // `interrupt_handle` calls this with BUDGET (64) and dropped up to
            // 63 of them with no trace. `limit` still bounds how many
            // descriptors are examined, which is where it earns its keep --
            // error frames `continue` without producing anything -- but nothing
            // is consumed that is not handed back.
            break;
        }

        info!(
            "RX DMA State: {:#x}, recv packets: {}",
            read_volatile((self.base + GETH_RX_DMA_STA) as *mut u32),
            rx_packets
        );

        if rxcount > 0 {
            info!(
                "######### RX Descriptor DMA: {:#x}",
                self.recv_ring.as_ptr() as usize
            );
            info!(
                "RX pointor: dirty: {}, clean: {}",
                self.rx_dirty, self.rx_clean
            );
            info!(
                "[0]: {:#x?} \ndesc: {:#x?}",
                self.recv_ring[0], self.recv_ring[desc_count]
            );
        }

        self.rx_refill();

        (buffer, rxcount as i32)
    }

    pub fn can_send(&mut self) -> bool {
        let avail_tx = if self.tx_clean >= (self.tx_dirty + 1) {
            self.tx_clean - (self.tx_dirty + 1)
        } else {
            DMA_DESC_TX - ((self.tx_dirty + 1) - self.tx_clean)
        };

        if avail_tx < 1 {
            error!("Tx Ring full !");
            return false;
        }
        /////////

        let desc = &self.send_ring[self.tx_dirty];
        invalidate_dcache(
            virt_to_phys(desc as *const DmaDesc as usize) as u64,
            size_of::<DmaDesc>() as u64,
        );
        if desc_get_own(desc) != 0 {
            return false;
        }

        let tx_status = read_volatile((self.base + GETH_TX_DMA_STA) as *mut u32) & 0b111;
        // from u-boot
        tx_status == 0b000 || tx_status == 0b110
    }

    pub fn geth_send(&mut self, send_buff: &[u8]) -> Result<i32, &str> {
        // Tx Ring full 判断一下？

        let mut entry = self.tx_dirty;
        //let mut first = &mut self.send_ring[entry];
        let first = entry;
        let mut desc = &mut self.send_ring[entry];
        let mut desc_count = entry;

        let csum_insert = 0; // 是否CHECKSUM_PARTIAL

        // linux驱动中的skb_headlen是什么?
        let mut len = send_buff.len() as u32;

        // Reject oversized frames BEFORE copying: each TX buffer is only
        // MAX_BUF_SZ bytes, but the copy below is sized to `send_buff.len()`, so
        // a frame larger than MAX_BUF_SZ would overflow the buffer (and the
        // multi-descriptor split that follows never fills the extra descriptors'
        // own buffers anyway). At a normal MTU this never triggers.
        if len > MAX_BUF_SZ {
            error!(
                "[rtl8211f] TX frame {} > {} (MAX_BUF_SZ); dropping",
                len, MAX_BUF_SZ
            );
            return Err("tx frame too large");
        }
        // Reject a zero-length (or sub-Ethernet) frame too: the descriptor
        // fill loop below is `while len != 0`, so with len == 0 it never runs.
        // The descriptor keeps the previous frame's length bits, OWN is set
        // anyway, the doorbell is rung, and `tx_dirty` is never advanced — so
        // `tx_complete` computes zero occupancy and never reclaims the slot.
        // The ring desynchronises permanently. Reachable from
        // `NetScheme::send` with an empty buffer (a 0-byte raw-socket write).
        if (len as usize) < 14 {
            error!(
                "[rtl8211f] TX frame {} bytes is shorter than an Ethernet header; dropping",
                len
            );
            return Err("tx frame too short");
        }

        // send buffer长度需要注意下, 应该2k左右
        let target = unsafe {
            slice::from_raw_parts_mut(self.send_buffers[entry] as *mut u8, send_buff.len())
        };
        target.copy_from_slice(send_buff);

        info!("========== TX PKT DATA: >>>>>>>>>>");
        print_hex_dump(target, 64);

        while len != 0 {
            // 注意结构体所有权的问题
            desc = &mut self.send_ring[entry];
            desc_count = entry;

            let tmp_len = if len > MAX_BUF_SZ { MAX_BUF_SZ } else { len };
            // dma_map_single()
            // 当要发送的包 > MAX_BUF_SZ时，循环可能会出问题？

            let paddr = desc.desc2;
            desc_buf_set(desc, paddr, tmp_len);

            /* Don't set the first's own bit, here */
            // (first != desc)
            if first != entry {
                //self.send_buffers[entry] = 0;
                desc_set_own(desc);
            }

            entry = (entry + 1) % DMA_DESC_TX;
            len -= tmp_len;
        }

        // 例外情况处理nfrags. 多数情况等于0？

        self.tx_dirty = entry;
        // desc_tx_close(first, desc, csum_insert);
        self.desc_tx_close(first, desc_count, csum_insert);

        desc_set_own(&mut self.send_ring[first]);

        // 再判断下环形缓冲区的空间

        // DMA store ordering: the payload MUST be visible in RAM before the NIC
        // can observe OWN=1, or it may DMA stale bytes onto the wire. Flush the
        // buffer, fence, flush the descriptor carrying OWN, then fence again so
        // the doorbell `tx_poll` is about to ring cannot reach the NIC ahead of
        // the descriptor it announces.
        //
        // The fourth operation used to be a second copy of the payload flush
        // instead of that closing fence -- the comment above it already
        // prescribed the fence -- so nothing ordered OWN=1 against the doorbell.
        // The size guard above keeps every frame inside one descriptor, so
        // `first` and `desc_count` are the same slot; each is named for what it
        // means rather than for the fact that they coincide.
        flush_cache(
            virt_to_phys(self.send_buffers[first]) as u64,
            send_buff.len() as u64,
        );
        fence_w();
        flush_cache(
            virt_to_phys(&self.send_ring[desc_count] as *const DmaDesc as usize) as u64,
            size_of::<DmaDesc>() as u64,
        );
        fence_w();

        info!(
            "######### TX Descriptor DMA: {:#x}",
            self.send_ring.as_ptr() as usize
        );
        info!(
            "TX pointor: dirty: {}, clean: {}",
            self.tx_dirty, self.tx_clean
        );
        info!(
            "[0]: {:#x?} \n[first]: {:#x?} \ndesc: {:#x?}",
            self.send_ring[0], self.send_ring[first], self.send_ring[desc_count]
        );

        info!(
            "TX DMA State: {:#x}",
            read_volatile((self.base + GETH_TX_DMA_STA) as *mut u32)
        );

        self.tx_poll();

        // 环形缓冲区的内存unmap之类的
        self.tx_complete();

        Ok(0)
    }

    pub fn rx_refill(&mut self) {
        while if self.rx_dirty >= (self.rx_clean + 1) {
            self.rx_dirty - (self.rx_clean + 1)
        } else {
            DMA_DESC_RX - ((self.rx_clean + 1) - self.rx_dirty)
            // (self.rx_dirty - (self.rx_clean + 1)) & (DMA_DESC_RX - 1)
        } > 0
        {
            info!(
                "rx_refill, rx_dirty: {}, rx_clean: {}",
                self.rx_dirty, self.rx_clean
            );

            let entry = self.rx_clean;
            let mut desc = &mut self.recv_ring[entry];

            /* From Linux driver
            if self.recv_buffers[entry] == 0 {
                //申请socket buffer空间, 大小MAX_BUF_SZ, 2K左右
                // netdev_alloc_skb_ip_align
                // dma_map_single

                // desc_buf_set
            }
            */

            let paddr = desc.desc2;
            desc_buf_set(desc, paddr, MAX_BUF_SZ);
            desc_set_own(desc);
            flush_cache(
                virt_to_phys(&self.recv_ring[entry] as *const DmaDesc as usize) as u64,
                size_of::<DmaDesc>() as u64,
            );

            // sync memery
            fence_w();

            self.rx_clean = (self.rx_clean + 1) % DMA_DESC_RX;
        }

        // If the RX DMA entered "Buffer Unavailable" state (ran out of
        // descriptors), writing bit 31 of GETH_RX_CTL1 triggers a
        // re-poll so the DMA resumes processing the newly-refilled ring.
        self.rx_poll();
    }

    pub fn tx_complete(&mut self) {
        let mut entry = 0;
        let mut tx_stat = 0;
        let mut tx_packets: u64 = 0;
        let mut tx_errors: u64 = 0;

        while if self.tx_dirty >= self.tx_clean {
            self.tx_dirty - self.tx_clean
        } else {
            DMA_DESC_TX - (self.tx_clean - self.tx_dirty)
            //(self.tx_dirty - self.tx_clean) & (DMA_DESC_TX - 1)
        } > 0
        {
            debug!(
                "tx_complete, tx_dirty: {}, tx_clean: {}",
                self.tx_dirty, self.tx_clean
            );

            entry = self.tx_clean;
            let mut desc = &mut self.send_ring[entry];

            invalidate_dcache(
                virt_to_phys(desc as *const DmaDesc as usize) as u64,
                size_of::<DmaDesc>() as u64,
            );
            if desc_get_own(desc) != 0 {
                warn!("tx_complete get desc own failed !");
                break;
            }

            if desc_get_tx_ls(desc) != 0 {
                // Underflow error, No carrier, Loss of collision
                if (desc.desc0 & ((0b1 << 1) | (0b11 << 10))) != 0 {
                    tx_stat = -1;
                }

                if tx_stat == 0 {
                    tx_packets += 1;
                } else {
                    tx_errors += 1;
                }
            }

            // dma_unmap_single

            //self.send_buffers[entry], clear 2k
            unsafe {
                slice::from_raw_parts_mut(self.send_buffers[entry] as *mut u8, 2048).fill(0);
            }
            flush_cache(virt_to_phys(self.send_buffers[entry]) as u64, 2048);

            // 注意不要把desc2的Buffer Addr清零了
            desc_init(desc);
            self.tx_clean = (entry + 1) % DMA_DESC_TX;
        }

        debug!("send packets: {}, send errors: {}", tx_packets, tx_errors);
    }

    // Enable and Restart Autonegotiation
    pub fn phy_restart_aneg(&mut self) {
        // Don't isolate the PHY if we're negotiating
        let _ = self.phy_modify(MII_BMCR, BMCR_ISOLATE, BMCR_ANENABLE | BMCR_ANRESTART);

        info!("Enable and Restart Autonegotiation ...");
        // NOLINK --> autoneg_complete --> set speed and duplex --> LINK

        let phyaddr = 0;
        let mut autoneg_complete: u32 = 0;
        let mut aneg_spins = 0u32;
        loop {
            // Read link and autonegotiation status
            let status = self.mdio_read(phyaddr, MII_BMSR);
            autoneg_complete = status & BMSR_ANEGCOMPLETE;
            //link = status & BMSR_LSTATUS;

            if autoneg_complete == BMSR_ANEGCOMPLETE {
                info!(
                    "Autonegotiation is completed ! autoneg_complete: {:#x}",
                    autoneg_complete
                );
                break;
            }
            // Bounded: a missing link partner never completes autoneg.
            aneg_spins += 1;
            if aneg_spins >= MDIO_MAX_SPINS {
                warn!("[rtl8211f] autonegotiation did not complete; continuing");
                break;
            }
        }
    }

    pub fn phy_modify(&mut self, regnum: u32, mask: u32, set: u32) -> Result<i32, &str> {
        let phyaddr = 0;
        let ret: u32 = self.mdio_read(phyaddr, regnum);
        /*
        if ret < 0 {
            return Err("mdio read error !"); }
        */

        let new: u32 = (ret & !mask) | set;
        info!("phy_modify, read: {:#x}, set: {:#x}", ret, new);

        if new == ret {
            return Ok(0);
        }

        self.mdio_write(phyaddr, regnum, new);

        Ok(0)
    }

    pub fn power_on(&mut self) {
        let mut value: u32 = read_volatile((self.base_phy + EMAC_EPHY_CLK_REG0) as *mut u32);
        value &= !(1 << 15); // select EXT_PHY

        write_volatile((self.base_phy + EMAC_EPHY_CLK_REG0) as *mut u32, value);
    }

    pub fn clk_enable(&mut self) {
        // reset_control_deassert()
        // 注, clock未初始化好的话，mdio read phy无法读到有效数据
        self.deassert_emac_reset();

        // enable ephy clk
        let mut value: u32 = read_volatile((self.base_ccu + EMAC_25M_CLK_REG) as *mut u32);
        value |= 0b11 << 30;
        write_volatile((self.base_ccu + EMAC_25M_CLK_REG) as *mut u32, value);

        // clk_prepare_enable()

        let mut clk_value: u32 = read_volatile((self.base_phy + EMAC_EPHY_CLK_REG0) as *mut u32);
        info!("clk enable, Read PHY CLK: {:#x}", clk_value);
        // RGMII接口，支持10/100/1000 Mbps速率
        if self.phy_mode == RGMII {
            clk_value |= 0x00000004; // set RGMII
        } else {
            clk_value &= !0x00000004;
        }

        clk_value &= !0x00002003; // clear RMII_EN, ETCS

        if (self.phy_mode == RGMII) || (self.phy_mode == GMII) {
            clk_value |= 0x00000002; // set ETCS=2

        // RMII接口，支持10/100 Mbps速率
        } else if self.phy_mode == RMII {
            clk_value |= 0x00002001;
        }

        // Adjust Tx/Rx clock delay
        clk_value &= !(0x07 << 10);
        clk_value |= (self.tx_delay & 0x07) << 10;
        clk_value &= !(0x1F << 5);
        clk_value |= (self.rx_delay & 0x1F) << 5;

        info!("clk enable, write clk value: {:#x}", clk_value);
        write_volatile((self.base_phy + EMAC_EPHY_CLK_REG0) as *mut u32, clk_value);
    }

    pub fn deassert_emac_reset(&mut self) {
        let mut value: u32 = read_volatile((self.base_ccu + EMAC_BGR_REG) as *mut u32);
        info!("Read CCU value: {:#x}", value);
        value &= !(1 << 16); // assert reset
        write_volatile((self.base_ccu + EMAC_BGR_REG) as *mut u32, value);

        value |= 1 << 16; // deassert reset
        value |= 1; // enable bus clock
        write_volatile((self.base_ccu + EMAC_BGR_REG) as *mut u32, value);
    }

    pub fn interrupt_status(&mut self) -> i32 {
        // int status register
        let mut intr_status: u32 = read_volatile((self.base + GETH_RGMII_STA) as *mut u32);
        if (intr_status & RGMII_IRQ) != 0 {
            read_volatile((self.base + GETH_RGMII_STA) as *mut u32);
        }
        intr_status = read_volatile((self.base + GETH_INT_STA) as *mut u32);
        info!("interrupt_handle, GETH_INT_STA: {:#x}", intr_status);

        let mut status = 0;
        // 不正常的中断
        if (intr_status & TX_UNF_INT) != 0 {
            status = TxDmaIrqStatus::TxHardErrorBumpTc as i32;
        }
        if (intr_status & TX_STOP_INT) != 0 {
            status = TxDmaIrqStatus::TxHardError as i32;
        }

        /* 正常的 TX/RX NORMAL interrupts */
        // (intr_status & (TX_INT | RX_INT | RX_EARLY_INT | TX_UA_INT)) != 0
        if (intr_status & (TX_INT | RX_INT)) != 0 {
            status = TxDmaIrqStatus::HandleTxRx as i32;
        }
        /* Clear the interrupt by writing a logic 1 to the CSR5[15-0] */
        write_volatile((self.base + GETH_INT_STA) as *mut u32, intr_status & 0x3FFF);

        status
    }

    pub fn interrupt_handle(&mut self, irq: u32, dev_id: &u32) -> Result<i32, &str> {
        let status = self.interrupt_status();

        // 处理
        if status == TxDmaIrqStatus::HandleTxRx as i32 {
            self.int_disable();
            // geth_poll()

            self.tx_complete(); // why? from Linux driver

            let (buffer, work_done) = self.geth_recv(BUDGET);
            if work_done < BUDGET as i32 {
                self.int_enable();
            }
        } else if status == TxDmaIrqStatus::TxHardError as i32 {
            error!("gmac interrupt handle tx error !");
        } else {
            info!("gmac interrupt handle status: {}, Do nothing ...", status);
        }

        Ok(1)
    }

    pub fn int_enable(&mut self) {
        info!("Int enable");
        write_volatile((self.base + GETH_INT_EN) as *mut u32, RX_INT | TX_UNF_INT);
    }

    pub fn int_disable(&mut self) {
        info!("Int disable");
        write_volatile((self.base + GETH_INT_EN) as *mut u32, 0);
    }

    pub fn desc_tx_close(&mut self, first: usize, end: usize, csum_insert: usize) {
        self.send_ring[first].desc1 |= 1 << 29; //First Segment,
        self.send_ring[end].desc1 |= 0b11 << 30; // Last Segment, Interrupt on completion

        if csum_insert != 0 {
            // Walk first..=end through the ring with modular indexing, bounded to
            // one full ring. The old version advanced a raw `*mut DmaDesc` with
            // `.add(1)`, which walked off the end of `send_ring` whenever the
            // segment wrapped (`end < first`) or `end` was the last index.
            let mut i = first;
            for _ in 0..DMA_DESC_TX {
                self.send_ring[i].desc1 |= 0b11 << 27;
                if i == end {
                    break;
                }
                i = (i + 1) % DMA_DESC_TX;
            }
        }
    }

    pub fn tx_poll(&self) {
        let value: u32 = read_volatile((self.base + GETH_TX_CTL1) as *mut u32);
        write_volatile((self.base + GETH_TX_CTL1) as *mut u32, value | 0x80000000);
    }

    pub fn rx_poll(&self) {
        let value: u32 = read_volatile((self.base + GETH_RX_CTL1) as *mut u32);
        write_volatile((self.base + GETH_RX_CTL1) as *mut u32, value | 0x80000000);
    }

    pub fn dma_init(&mut self) {
        write_volatile((self.base + GETH_BASIC_CTL1) as *mut u32, 8 << 24); // burst
                                                                            // 打开网卡中断
        self.int_enable();
    }

    pub fn mac_reset(&mut self) -> Result<i32, &str> {
        let mut mac_reset_value: u32 = read_volatile((self.base + GETH_BASIC_CTL1) as *mut u32);
        mac_reset_value |= SOFT_RST;
        write_volatile((self.base + GETH_BASIC_CTL1) as *mut u32, mac_reset_value);

        // 原子上下文的等待
        //udelay(10000);
        {
            let mut spins = 0u32;
            while (SOFT_RST & read_volatile((self.base + GETH_BASIC_CTL1) as *mut u32)) != 0 {
                spins += 1;
                if spins >= MDIO_MAX_SPINS {
                    warn!("[rtl8211f] GMAC soft reset did not clear; continuing");
                    break;
                }
            }
        }

        let value = read_volatile((self.base + GETH_BASIC_CTL1) as *mut u32);
        info!("Read BASIC CTL1: {:#x}", value);
        if (value & SOFT_RST) == 0 {
            info!("Soft reset operation is completed !");
            Ok((value & SOFT_RST) as i32)
        } else {
            error!("Soft reset operation is NOT completed !");
            Err("mac Soft Reset failed !")
        }
    }

    pub fn mac_init(&mut self, txmode: usize, rxmode: usize) {
        self.dma_init();

        /* Initialize the core component */
        let mut value: u32 = read_volatile((self.base + GETH_TX_CTL0) as *mut u32);
        value |= 1 << 30; /* Jabber Disable */
        write_volatile((self.base + GETH_TX_CTL0) as *mut u32, value);
        info!("mac init, write TX_CTL0 {:#x}", value);

        let mut value: u32 = read_volatile((self.base + GETH_RX_CTL0) as *mut u32);
        value &= !(1 << 27); /* Disable CRC & IPv4 Header Checksum */
        value &= !(1 << 28); /* Keep Pad/CRC */
        value |= 1 << 29; /* Jumbo Frame Enable */
        write_volatile((self.base + GETH_RX_CTL0) as *mut u32, value);
        info!("mac init, write RX_CTL0 {:#x}", value);

        write_volatile(
            (self.base + GETH_MDIO_ADDR) as *mut u32,
            MDC_CLOCK_RATIO << 20,
        ); /* MDC_DIV_RATIO */

        /* Set the Rx&Tx mode */
        let mut value: u32 = read_volatile((self.base + GETH_TX_CTL1) as *mut u32);

        if txmode == SF_DMA_MODE {
            value |= TX_MD;
            value |= TX_NEXT_FRM;
        } else {
            value &= !TX_MD;
            value &= !TX_TH;
            /* Set the transmit threshold */
            if txmode <= 64 {
                value |= 0x00000000;
            } else if txmode <= 128 {
                value |= 0x00000100;
            } else if txmode <= 192 {
                value |= 0x00000200;
            } else {
                value |= 0x00000300;
            }
        }
        write_volatile((self.base + GETH_TX_CTL1) as *mut u32, value);
        info!("mac init, write TX_CTL1 {:#x}", value);

        let mut value: u32 = read_volatile((self.base + GETH_RX_CTL1) as *mut u32);
        // SF_DMA_MODE
        if rxmode == SF_DMA_MODE {
            value |= RX_MD;
        } else {
            value &= !RX_MD;
            value &= !RX_TH;
            if rxmode <= 32 {
                value |= 0x10;
            } else if rxmode <= 64 {
                value |= 0x00;
            } else if rxmode <= 96 {
                value |= 0x20;
            } else {
                value |= 0x30;
            }
        }
        /* Forward frames with error and undersized good frame. */
        value |= RX_ERR_FRM | RX_RUNT_FRM;
        write_volatile((self.base + GETH_RX_CTL1) as *mut u32, value);
        info!("mac init, write RX_CTL1 {:#x}", value);
    }

    pub fn mac_enable(&mut self) {
        let mut value: u32 = read_volatile((self.base + GETH_TX_CTL0) as *mut u32);
        value |= 1 << 31;
        write_volatile((self.base + GETH_TX_CTL0) as *mut u32, value);

        let mut value: u32 = read_volatile((self.base + GETH_RX_CTL0) as *mut u32);
        value |= 1 << 31;
        write_volatile((self.base + GETH_RX_CTL0) as *mut u32, value);
    }

    pub fn mac_disable(&mut self) {
        let mut value: u32 = read_volatile((self.base + GETH_TX_CTL0) as *mut u32);
        value &= !(1 << 31);
        write_volatile((self.base + GETH_TX_CTL0) as *mut u32, value);

        let mut value: u32 = read_volatile((self.base + GETH_RX_CTL0) as *mut u32);
        value &= !(1 << 31);
        write_volatile((self.base + GETH_RX_CTL0) as *mut u32, value);
    }

    pub fn get_umac(&self) -> [u8; 6] {
        self.mac
    }

    pub fn set_umac(&self, addr: &[u8; 6], index: u32) {
        info!(
            "Read mac addr high0 and low0: {:#x} {:#x}",
            read_volatile((self.base + GETH_ADDR_HI) as *mut u32),
            read_volatile((self.base + GETH_ADDR_LO) as *mut u32)
        );

        let data: u32 = ((addr[5] as u32) << 8) | (addr[4] as u32);
        write_volatile((self.base + GETH_ADDR_HI + (index << 3)) as *mut u32, data);
        let data: u32 = ((addr[3] as u32) << 24)
            | ((addr[2] as u32) << 16)
            | ((addr[1] as u32) << 8)
            | (addr[0] as u32);
        write_volatile((self.base + GETH_ADDR_LO + (index << 3)) as *mut u32, data);
    }

    pub fn start_rx(&mut self, rxbase: u32) {
        //rxbase需要32位对齐
        write_volatile((self.base + GETH_RX_DESC_LIST) as *mut u32, rxbase);

        let mut value: u32 = read_volatile((self.base + GETH_RX_CTL1) as *mut u32);
        value |= 0x40000000;
        write_volatile((self.base + GETH_RX_CTL1) as *mut u32, value);
    }

    pub fn stop_rx(&mut self) {
        let mut value: u32 = read_volatile((self.base + GETH_RX_CTL1) as *mut u32);
        value &= !0x40000000;
        write_volatile((self.base + GETH_RX_CTL1) as *mut u32, value);
    }

    pub fn start_tx(&mut self, txbase: u32) {
        //txbase需要32位对齐
        write_volatile((self.base + GETH_TX_DESC_LIST) as *mut u32, txbase);

        let mut value: u32 = read_volatile((self.base + GETH_TX_CTL1) as *mut u32);
        value |= 0x40000000;
        write_volatile((self.base + GETH_TX_CTL1) as *mut u32, value);
    }

    pub fn stop_tx(&mut self) {
        let mut value: u32 = read_volatile((self.base + GETH_TX_CTL1) as *mut u32);
        value &= !0x40000000;
        write_volatile((self.base + GETH_TX_CTL1) as *mut u32, value);
    }

    pub fn pinctrl_gpio_set(&mut self, offset: u32, value: u32) {
        // 0x0 <= offset <= 0x0350
        assert!(
            offset <= 0x0350,
            "Invalid gpio register offset: {:#x}",
            offset
        );

        let mut regval: u32 = read_volatile((self.pinctrl + offset) as *mut u32);
        //regval |= value;
        info!("GPIO offset: {:#x}, read regval: {:#x}", offset, regval);
        write_volatile((self.pinctrl + offset) as *mut u32, value);
    }

    pub fn hash_filter(&mut self, low: u32, high: u32) {
        info!("RX hash filter low: {:#x}, high: {:#x}", low, high);

        write_volatile((self.base + GETH_RX_HASH0) as *mut u32, high);
        write_volatile((self.base + GETH_RX_HASH1) as *mut u32, low);
    }

    pub fn set_filter(&mut self, flags: u64) {
        let mut tmp_flags: u32 = 0;

        tmp_flags |= ((flags >> 31)
            | ((flags >> 9) & 0x00000002)
            | ((flags << 1) & 0x00000010)
            | ((flags >> 3) & 0x00000060)
            | ((flags << 7) & 0x00000300)
            | ((flags << 6) & 0x00003000)
            | ((flags << 12) & 0x00030000)
            | (flags << 31)) as u32;

        info!(
            "Set RX frame filter, flags: {:#x}, write value: {:#x}",
            flags, tmp_flags
        );
        write_volatile((self.base + GETH_RX_FRM_FLT) as *mut u32, tmp_flags);
    }

    pub fn set_link_mode(&mut self, duplex: i32, speed: i32) {
        let mut ctrl: u32 = read_volatile((self.base + GETH_BASIC_CTL0) as *mut u32);
        if duplex == 0 {
            ctrl &= !(CTL0_DM);
        } else {
            ctrl |= CTL0_DM;
        }

        match speed {
            1000 => ctrl &= !0x0C,
            _ => {
                ctrl |= 0x08;
                if speed == 100 {
                    ctrl |= 0x04;
                } else {
                    ctrl &= !0x04;
                }
            }
        }

        write_volatile((self.base + GETH_BASIC_CTL0) as *mut u32, ctrl);

        let value = read_volatile((self.base + GETH_BASIC_CTL0) as *mut u32);
        info!(
            "Set link mode:  duplex {}, speed {}, CTL0: {:#x}",
            duplex, speed, value
        );
    }

    pub fn mac_loopback(&mut self, enable: u32) {
        let mut reg: u32 = read_volatile((self.base + GETH_BASIC_CTL0) as *mut u32);
        if enable != 0 {
            reg |= 0x02;
        } else {
            reg &= !0x02;
        }
        write_volatile((self.base + GETH_BASIC_CTL0) as *mut u32, reg);
    }

    pub fn flow_ctrl(&mut self, duplex: i32, fc: u32, pause: u32) {
        let mut flow: u32 = 0;
        info!(
            "Set flow ctrl: duplex {}, fc {}, pause {}",
            duplex, fc, pause
        );

        if fc & FLOW_RX != 0 {
            flow = read_volatile((self.base + GETH_RX_CTL0) as *mut u32);
            flow |= 0x10000;
            write_volatile((self.base + GETH_RX_CTL0) as *mut u32, flow);
        }

        if fc & FLOW_TX != 0 {
            flow = read_volatile((self.base + GETH_TX_FLOW_CTL) as *mut u32);
            flow |= 0x00001;
            write_volatile((self.base + GETH_TX_FLOW_CTL) as *mut u32, flow);
        }

        if duplex != 0 {
            flow = read_volatile((self.base + GETH_TX_FLOW_CTL) as *mut u32);
            flow |= pause << 4;
            write_volatile((self.base + GETH_TX_FLOW_CTL) as *mut u32, flow);
        }
    }

    pub fn mdio_read(&mut self, phyaddr: u32, phyreg: u32) -> u32 {
        let mut value: u32 = 0;

        value |= (MDC_CLOCK_RATIO & 0x07) << 20;

        value |= ((phyaddr << 12) & (0x0001F000)) | ((phyreg << 4) & (0x000007F0)) | MII_BUSY;

        {
            let mut spins = 0u32;
            while (read_volatile((self.base + GETH_MDIO_ADDR) as *mut u32) & MII_BUSY) == 1 {
                spins += 1;
                if spins >= MDIO_MAX_SPINS {
                    break;
                }
            }
        }

        write_volatile((self.base + GETH_MDIO_ADDR) as *mut u32, value);

        {
            let mut spins = 0u32;
            while (read_volatile((self.base + GETH_MDIO_ADDR) as *mut u32) & MII_BUSY) == 1 {
                spins += 1;
                if spins >= MDIO_MAX_SPINS {
                    break;
                }
            }
        }

        //16位有效
        let ret = read_volatile((self.base + GETH_MDIO_DATA) as *mut u32);
        // info!("mdio_read MDIO DATA: {:#x}", ret);

        ret
    }

    pub fn mdio_write(&mut self, phyaddr: u32, phyreg: u32, data: u32) {
        let mut value: u32 = (0x07 << 20) & read_volatile((self.base + GETH_MDIO_ADDR) as *mut u32)
            | (MDC_CLOCK_RATIO << 20);

        value |= (((phyaddr << 12) & (0x0001F000)) | ((phyreg << 4) & (0x000007F0)))
            | MII_WRITE
            | MII_BUSY;

        {
            let mut spins = 0u32;
            while (read_volatile((self.base + GETH_MDIO_ADDR) as *mut u32) & MII_BUSY) == 1 {
                spins += 1;
                if spins >= MDIO_MAX_SPINS {
                    break;
                }
            }
        }

        write_volatile((self.base + GETH_MDIO_DATA) as *mut u32, data);
        write_volatile((self.base + GETH_MDIO_ADDR) as *mut u32, value);

        {
            let mut spins = 0u32;
            while (read_volatile((self.base + GETH_MDIO_ADDR) as *mut u32) & MII_BUSY) == 1 {
                spins += 1;
                if spins >= MDIO_MAX_SPINS {
                    break;
                }
            }
        }
    }

    pub fn mdio_reset(&mut self) {
        write_volatile((self.base + GETH_MDIO_ADDR) as *mut u32, 4 << 2);
    }
}

pub fn desc_set_own(desc: &mut DmaDesc) {
    desc.desc0 |= 0x80000000;
}

pub fn desc_get_own(desc: &DmaDesc) -> u32 {
    desc.desc0 & 0x80000000
}

pub fn desc_get_tx_ls(desc: &DmaDesc) -> u32 {
    desc.desc1 & 0x40000000 // Last Segment
}

pub fn desc_buf_set(desc: &mut DmaDesc, paddr: u32, size: u32) {
    desc.desc1 &= !((1 << 11) - 1);
    desc.desc1 |= size & ((1 << 11) - 1);
    desc.desc2 = paddr;
}

pub fn desc_init(desc: &mut DmaDesc) {
    desc.desc1 = 0;
    desc.desc1 |= 1 << 24;

    // 这里用的Buffer Addr不发生改变
    //desc.desc2 = 0;
}

/// Where a device register lives.
///
/// On the D1 this is the kernel's physical-to-virtual map. In a host test build
/// there is no such map -- `phys_to_virt` is the identity there -- so the four
/// register blocks become ordinary memory and this is where the driver's
/// hardcoded device addresses get pointed at them. See [`fake`].
#[cfg(not(test))]
fn mmio(phys: usize) -> usize {
    phys_to_virt(phys)
}

#[cfg(test)]
fn mmio(phys: usize) -> usize {
    fake::translate(phys)
}

fn read_volatile<T>(src: *const T) -> T {
    unsafe { core::ptr::read_volatile(mmio(src as usize) as *const T) }
}

fn write_volatile<T>(dst: *mut T, value: T) {
    unsafe {
        core::ptr::write_volatile(mmio(dst as usize) as *mut T, value);
    }
    // A register the driver posts a command to answers back. On real hardware
    // that happens on its own; the fake has to be told a store landed.
    #[cfg(test)]
    if size_of::<T>() == size_of::<u32>() {
        fake::posted(dst as usize);
    }
}

pub fn print_hex_dump(buf: &[u8], len: usize) {}

/// The GMAC and its MDIO bus, faked over ordinary memory.
///
/// `phys_to_virt` is the identity in a host build, so without this the first
/// `read_volatile` in the driver would dereference the literal address
/// `0x0450_0048`. Each of the four register blocks the driver touches gets one
/// page of real memory, and an address outside all four panics by name instead
/// of quietly landing in whatever was there.
///
/// Two of those registers are not just storage. The driver posts an MDIO
/// transaction by setting `MII_BUSY` and then spins until the hardware clears
/// it, and it starts a MAC soft reset by setting `SOFT_RST` and spinning until
/// the MAC clears that; a purely passive fake would spin `MDIO_MAX_SPINS` times
/// on every single register access. [`posted`] completes both the way the
/// hardware does -- and the switches on [`Guard`] make each one *not* complete,
/// which is the only way to reach the driver's give-up paths.
#[cfg(test)]
pub(crate) mod fake {
    extern crate std;

    use super::*;

    /// One page each, which is more than any of the four blocks really uses, so
    /// a stray access stays inside its own window where a test can see it.
    const WINDOW_LEN: usize = 0x1000;
    const BASES: [u32; 4] = [GMAC_BASE, CCU_BASE, SYS_CFG_BASE, PINCTRL_GPIO_BASE];

    fn windows() -> &'static [usize; 4] {
        static WINDOWS: spin::Once<[usize; 4]> = spin::Once::new();
        WINDOWS.call_once(|| {
            let mut out = [0usize; 4];
            for slot in out.iter_mut() {
                *slot = alloc::vec![0u8; WINDOW_LEN].leak().as_mut_ptr() as usize;
            }
            out
        })
    }

    /// Where a device address lives on the host.
    pub fn translate(phys: usize) -> usize {
        for (i, base) in BASES.iter().enumerate() {
            let base = *base as usize;
            if phys >= base && phys < base + WINDOW_LEN {
                return windows()[i] + (phys - base);
            }
        }
        panic!(
            "rtl8211f touched {:#x}, which is in no register window",
            phys
        );
    }

    pub fn read(phys: u32) -> u32 {
        unsafe { (translate(phys as usize) as *const u32).read_volatile() }
    }

    pub fn write(phys: u32, value: u32) {
        unsafe { (translate(phys as usize) as *mut u32).write_volatile(value) }
    }

    /// The PHY, as the 32 MII registers the driver reads it through. The link
    /// partner's abilities show up in `MII_LPA` and `MII_STAT1000`, which is
    /// where autonegotiation leaves them.
    #[derive(Clone, Copy)]
    pub struct Phy {
        pub regs: [u32; 32],
    }

    impl Phy {
        /// A gigabit PHY facing a partner that offers every speed.
        pub fn gigabit_partner() -> Self {
            let mut regs = [0u32; 32];
            regs[MII_BMSR as usize] =
                BMSR_LSTATUS | BMSR_ANEGCOMPLETE | BMSR_ANEGCAPABLE | BMSR_ESTATEN;
            regs[MII_LPA as usize] = LPA_100FULL | LPA_100HALF | LPA_10FULL | LPA_10HALF;
            regs[MII_STAT1000 as usize] = LPA_1000FULL | LPA_1000HALF;
            Self { regs }
        }

        /// A PHY with no gigabit at all: no extended status, and `MII_STAT1000`
        /// reads back as the reserved register it is. All ones is what a real
        /// one often gives, and it carries `LPA_1000MSFAIL`.
        pub fn fast_ethernet_only() -> Self {
            let mut regs = [0u32; 32];
            regs[MII_BMSR as usize] = BMSR_LSTATUS | BMSR_ANEGCOMPLETE | BMSR_ANEGCAPABLE;
            regs[MII_LPA as usize] = LPA_100FULL | LPA_100HALF;
            regs[MII_STAT1000 as usize] = 0xffff;
            Self { regs }
        }

        /// No cable, or a partner that never answers.
        pub fn no_link(mut self) -> Self {
            self.regs[MII_BMSR as usize] &= !BMSR_LSTATUS;
            self
        }

        /// The partner offers symmetric 802.3x pause.
        pub fn offering_pause(mut self) -> Self {
            self.regs[MII_LPA as usize] |= LPA_PAUSE_CAP | LPA_PAUSE_ASYM;
            self
        }

        /// The partner asks for pause in one direction only.
        pub fn offering_asymmetric_pause_only(mut self) -> Self {
            self.regs[MII_LPA as usize] |= LPA_PAUSE_ASYM;
            self.regs[MII_LPA as usize] &= !LPA_PAUSE_CAP;
            self
        }
    }

    static PHY: spin::Mutex<Option<Phy>> = spin::Mutex::new(None);
    /// The PHY's `BMCR_RESET` never clears: a PHY with no clock behind it.
    static PHY_RESET_STICKS: core::sync::atomic::AtomicBool =
        core::sync::atomic::AtomicBool::new(false);
    /// The MAC's `SOFT_RST` never clears: EMAC clock or power not up yet.
    static MAC_RESET_STICKS: core::sync::atomic::AtomicBool =
        core::sync::atomic::AtomicBool::new(false);

    pub fn phy_reg(reg: u32) -> u32 {
        PHY.lock().as_ref().map_or(0, |p| p.regs[reg as usize])
    }

    /// Complete whatever the driver just posted.
    ///
    /// Called right after a register store, so the value is read back out of the
    /// window rather than threaded through the generic `write_volatile`.
    pub(super) fn posted(phys: usize) {
        use core::sync::atomic::Ordering;

        if phys == (GMAC_BASE + GETH_BASIC_CTL1) as usize {
            let ctl = read(GMAC_BASE + GETH_BASIC_CTL1);
            if ctl & SOFT_RST != 0 && !MAC_RESET_STICKS.load(Ordering::SeqCst) {
                // The MAC clears SOFT_RST when the reset completes.
                write(GMAC_BASE + GETH_BASIC_CTL1, ctl & !SOFT_RST);
            }
            return;
        }
        if phys != (GMAC_BASE + GETH_MDIO_ADDR) as usize {
            return;
        }
        let cmd = read(GMAC_BASE + GETH_MDIO_ADDR);
        if cmd & MII_BUSY == 0 {
            return;
        }
        let reg = ((cmd & 0x0000_07F0) >> 4) as usize;
        {
            let mut guard = PHY.lock();
            if let Some(phy) = guard.as_mut() {
                if cmd & MII_WRITE != 0 {
                    phy.regs[reg] = read(GMAC_BASE + GETH_MDIO_DATA) & 0xffff;
                    if reg == MII_BMCR as usize && !PHY_RESET_STICKS.load(Ordering::SeqCst) {
                        // A real PHY finishes its reset and clears the bit itself.
                        phy.regs[reg] &= !BMCR_RESET;
                    }
                } else {
                    write(GMAC_BASE + GETH_MDIO_DATA, phy.regs[reg]);
                }
            }
        }
        // Transaction done: the GMAC clears MII_BUSY.
        write(GMAC_BASE + GETH_MDIO_ADDR, cmd & !MII_BUSY);
    }

    /// Zero every register window, install `phy`, and hand back the guard that
    /// serialises the whole fake. The register file, the PHY and the cache log
    /// are one process-global apiece and this crate's tests run in parallel, so
    /// they all sit behind this one lock -- one lock, so there is no pair to take
    /// in the wrong order.
    pub fn with_phy(phy: Phy) -> Guard {
        use core::sync::atomic::Ordering;

        static TURNSTILE: spin::Mutex<()> = spin::Mutex::new(());
        let turnstile = TURNSTILE.lock();
        for w in windows().iter() {
            unsafe { core::ptr::write_bytes(*w as *mut u8, 0, WINDOW_LEN) };
        }
        *PHY.lock() = Some(phy);
        PHY_RESET_STICKS.store(false, Ordering::SeqCst);
        MAC_RESET_STICKS.store(false, Ordering::SeqCst);
        Guard {
            _turnstile: turnstile,
        }
    }

    pub struct Guard {
        _turnstile: spin::MutexGuard<'static, ()>,
    }

    impl Guard {
        /// The PHY's `BMCR_RESET` will never clear.
        pub fn with_phy_stuck_in_reset(self) -> Self {
            PHY_RESET_STICKS.store(true, core::sync::atomic::Ordering::SeqCst);
            self
        }

        /// The MAC's `SOFT_RST` will never clear.
        pub fn with_mac_stuck_in_reset(self) -> Self {
            MAC_RESET_STICKS.store(true, core::sync::atomic::Ordering::SeqCst);
            self
        }

        /// Start recording cache and fence operations, from empty.
        pub fn record_cache(&self) {
            super::super::utils::host::begin();
        }

        /// What the driver has asked the cache to do since [`Self::record_cache`].
        pub fn cache_ops(&self) -> alloc::vec::Vec<super::super::utils::host::CacheOp> {
            super::super::utils::host::ops()
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            use core::sync::atomic::Ordering;
            *PHY.lock() = None;
            PHY_RESET_STICKS.store(false, Ordering::SeqCst);
            MAC_RESET_STICKS.store(false, Ordering::SeqCst);
            super::super::utils::host::end();
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::super::utils::host::CacheOp;
    use super::fake::{self, Phy};
    use super::*;
    use crate::net::ProviderImpl;

    /// A driver with its rings allocated out of host memory. `new` touches no
    /// registers, only DMA, so it needs nothing from the fake device.
    fn driver() -> RTL8211F<ProviderImpl> {
        RTL8211F::new(&[0x02, 0x11, 0x22, 0x33, 0x44, 0x55])
    }

    /// Leave a frame in RX descriptor `slot` the way the GMAC leaves one: OWN
    /// clear, the last-descriptor bit set, no error bits, and the length the
    /// hardware reports -- which counts the 4-byte FCS the driver strips.
    fn stage_rx(nic: &mut RTL8211F<ProviderImpl>, slot: usize, on_wire: u32, bytes: &[u8]) {
        let buf = nic.recv_buffers[slot];
        unsafe {
            slice::from_raw_parts_mut(buf as *mut u8, bytes.len()).copy_from_slice(bytes);
        }
        nic.recv_ring[slot].desc0 = (on_wire << 16) | (1 << 8);
    }

    fn stage_rx_frame(nic: &mut RTL8211F<ProviderImpl>, slot: usize, payload: &[u8]) {
        stage_rx(nic, slot, payload.len() as u32 + 4, payload);
    }

    // ------------------------------------------------------------------ 802.3x

    #[test]
    fn the_pause_the_driver_advertises_is_the_pause_it_later_asks_about() {
        let _dev = fake::with_phy(Phy::gigabit_partner().offering_pause());
        let mut nic = driver();
        let _ = nic.adjust_link();

        let advertised = fake::phy_reg(MII_ADVERTISE);
        assert_ne!(
            advertised & ADVERTISE_PAUSE_CAP,
            0,
            "the driver resolves flow control out of `MII_LPA & MII_ADVERTISE`, so \
             clearing the pause bit it advertises makes that answer 'no pause' \
             whatever the partner offered: advertised {:#x}",
            advertised
        );
    }

    #[test]
    fn a_partner_that_offers_pause_gets_flow_control_switched_on() {
        let _dev = fake::with_phy(Phy::gigabit_partner().offering_pause());
        let mut nic = driver();
        nic.adjust_link()
            .expect("a gigabit partner offering everything must link");

        assert_ne!(
            fake::read(GMAC_BASE + GETH_RX_CTL0) & 0x1_0000,
            0,
            "RX flow control must be on, or the MAC ignores the pause frames the \
             partner just said it would send"
        );
        assert_ne!(
            fake::read(GMAC_BASE + GETH_TX_FLOW_CTL) & 0x1,
            0,
            "TX flow control must be on, or the MAC cannot ask the partner to stop \
             and a download stalls instead of throttling"
        );
        assert_eq!(
            fake::read(GMAC_BASE + GETH_TX_FLOW_CTL) >> 4 & 0xffff,
            PAUSE_TIME,
            "the pause quanta the GMAC sends must be PAUSE_TIME"
        );
    }

    #[test]
    fn a_partner_that_offers_no_pause_leaves_flow_control_off() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let mut nic = driver();
        nic.adjust_link().expect("the link must still come up");

        assert_eq!(
            fake::read(GMAC_BASE + GETH_RX_CTL0) & 0x1_0000,
            0,
            "a partner that never offered pause must not have it turned on"
        );
        assert_eq!(
            fake::read(GMAC_BASE + GETH_TX_FLOW_CTL),
            0,
            "nothing at all belongs in the flow-control register with pause off"
        );
    }

    #[test]
    fn a_partner_that_offers_only_asymmetric_pause_gets_one_direction() {
        let _dev = fake::with_phy(Phy::gigabit_partner().offering_asymmetric_pause_only());
        let mut nic = driver();
        nic.adjust_link().expect("the link must come up");

        assert_ne!(
            fake::read(GMAC_BASE + GETH_RX_CTL0) & 0x1_0000,
            0,
            "we advertise symmetric pause, so an asymmetric partner is asking us to \
             honour its pause frames (IEEE 802.3 Annex 28B)"
        );
        assert_eq!(
            fake::read(GMAC_BASE + GETH_TX_FLOW_CTL) & 0x1,
            0,
            "it did not ask us to send pause frames, so TX flow control stays off"
        );
    }

    // -------------------------------------------------------- speed resolution

    #[test]
    fn a_phy_without_gigabit_is_not_asked_to_resolve_a_gigabit_link() {
        let _dev = fake::with_phy(Phy::fast_ethernet_only());
        let mut nic = driver();
        let outcome = nic.adjust_link();
        assert!(
            outcome.is_ok(),
            "MII_STAT1000 is a reserved register on a 100M PHY; reading it as a \
             negotiation result turns a good 100M link into {:?}",
            outcome.err()
        );

        assert_eq!(
            fake::read(GMAC_BASE + GETH_BASIC_CTL0) & 0x0f,
            CTL0_DM | 0x08 | 0x04,
            "a 100M full-duplex link: duplex bit, not-gigabit, 100 rather than 10"
        );
    }

    #[test]
    fn a_gigabit_partner_is_programmed_as_a_gigabit_link() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let mut nic = driver();
        nic.adjust_link().expect("a gigabit partner must link");

        assert_eq!(
            fake::read(GMAC_BASE + GETH_BASIC_CTL0) & 0x0f,
            CTL0_DM,
            "gigabit is the speed bits CLEARED, with the duplex bit kept"
        );
    }

    #[test]
    fn with_no_cable_the_link_speed_is_never_programmed() {
        let _dev = fake::with_phy(Phy::gigabit_partner().no_link());
        let mut nic = driver();
        let _ = nic.adjust_link();

        assert_eq!(
            fake::read(GMAC_BASE + GETH_BASIC_CTL0),
            0,
            "with no carrier the driver must not program a speed it guessed"
        );
    }

    // ------------------------------------------------------------ DMA ordering

    #[test]
    fn the_descriptor_that_publishes_a_frame_is_fenced_against_the_doorbell() {
        let dev = fake::with_phy(Phy::gigabit_partner());
        let mut nic = driver();
        let frame = [0xa5u8; 64];

        let payload = virt_to_phys(nic.send_buffers[0]) as u64;
        let descriptor = virt_to_phys(&nic.send_ring[0] as *const DmaDesc as usize) as u64;

        dev.record_cache();
        nic.geth_send(&frame).expect("a 64-byte frame must go out");
        let ops = dev.cache_ops();

        assert!(ops.len() >= 4, "too few cache operations: {:?}", ops);
        assert_eq!(
            &ops[..4],
            &[
                CacheOp::Flush(payload, frame.len() as u64),
                CacheOp::Fence,
                CacheOp::Flush(descriptor, size_of::<DmaDesc>() as u64),
                CacheOp::Fence,
            ][..],
            "publishing a frame is payload, fence, descriptor, fence: without that \
             last fence nothing orders OWN=1 against the doorbell that follows, and \
             the NIC can DMA a descriptor whose payload is still in the store buffer"
        );
    }

    // --------------------------------------------------------------- receiving

    #[test]
    fn a_frame_taken_out_of_the_ring_is_always_the_frame_handed_back() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let mut nic = driver();
        let first = [0x11u8; 60];
        let second = [0x22u8; 70];
        stage_rx_frame(&mut nic, 0, &first);
        stage_rx_frame(&mut nic, 1, &second);

        // A budget of eight, two frames waiting.
        let (got, count) = nic.geth_recv(8);
        assert_eq!(
            got,
            &first[..],
            "the first frame is the one that comes back; the loop used to keep \
             going and overwrite it, so a budget above 1 consumed frames out of \
             the ring and returned only the last"
        );
        assert_eq!(count, 1, "one frame consumed, one frame handed over");

        let (got, _) = nic.geth_recv(8);
        assert_eq!(
            got,
            &second[..],
            "the second frame must still be in the ring, not dropped"
        );
    }

    #[test]
    fn a_descriptor_claiming_more_than_the_buffer_holds_is_clamped_to_it() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let mut nic = driver();
        // 0x3fff is the largest the descriptor's 14-bit length field can say, and
        // on the last descriptor of a multi-descriptor frame the hardware really
        // does report the WHOLE frame length there -- more than one buffer holds.
        stage_rx(&mut nic, 0, 0x3fff, &[0x5au8; 64]);

        let (got, _) = nic.geth_recv(1);
        assert_eq!(
            got.len(),
            (MAX_BUF_SZ - 4) as usize,
            "the frame handed back must be clamped to the DMA buffer: taking the \
             reported length at face value reads -- and invalidates cache over -- \
             far past the end of a one-page RX buffer"
        );
    }

    #[test]
    fn a_runt_too_short_to_hold_its_own_checksum_is_dropped() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let mut nic = driver();
        // Three bytes on the wire: less than the 4-byte FCS the driver strips.
        stage_rx(&mut nic, 0, 3, &[0xde, 0xad, 0xbe]);

        let (got, _) = nic.geth_recv(1);
        assert!(
            got.is_empty(),
            "a frame shorter than its FCS must be dropped, not have 4 subtracted \
             from its length"
        );
    }

    // ------------------------------------------------------------ transmitting

    #[test]
    fn a_frame_shorter_than_an_ethernet_header_is_refused_without_moving_the_ring() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let mut nic = driver();
        assert!(
            nic.geth_send(&[]).is_err(),
            "an empty frame must be refused"
        );
        assert!(
            nic.geth_send(&[0u8; 13]).is_err(),
            "13 bytes cannot carry an Ethernet header"
        );
        assert_eq!(
            (nic.tx_dirty, nic.tx_clean),
            (0, 0),
            "a refused frame must leave the ring pointers where they were, or the \
             slot is never reclaimed and the ring desynchronises for good"
        );
    }

    #[test]
    fn a_frame_too_big_for_a_dma_buffer_is_refused_without_moving_the_ring() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let mut nic = driver();
        let huge = alloc::vec![0u8; MAX_BUF_SZ as usize + 1];
        assert!(
            nic.geth_send(&huge).is_err(),
            "a frame past MAX_BUF_SZ would overflow the TX buffer it is copied into"
        );
        assert_eq!((nic.tx_dirty, nic.tx_clean), (0, 0));
    }

    #[test]
    fn a_tx_ring_with_no_room_says_so_and_an_empty_one_does_not() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let mut nic = driver();

        assert!(nic.can_send(), "an untouched ring has room");

        // One slot short of the write cursor: every descriptor is outstanding.
        nic.tx_clean = 0;
        nic.tx_dirty = DMA_DESC_TX - 1;
        assert!(
            !nic.can_send(),
            "with the write cursor one slot behind the reclaim cursor the ring is \
             full, and claiming otherwise overwrites a descriptor the NIC owns"
        );

        nic.tx_clean = 5;
        nic.tx_dirty = 5;
        assert!(nic.can_send(), "both cursors together is an empty ring");
    }

    // ------------------------------------------------------------- bringing up

    #[test]
    fn bringing_the_gmac_up_enables_it_and_hands_the_ring_to_the_dma() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let mut nic = driver();
        nic.open().expect("a healthy GMAC must come up");

        assert_ne!(
            fake::read(GMAC_BASE + GETH_TX_CTL0) & (1 << 31),
            0,
            "TX must be enabled"
        );
        assert_ne!(
            fake::read(GMAC_BASE + GETH_RX_CTL0) & (1 << 31),
            0,
            "RX must be enabled"
        );
        assert_eq!(
            fake::read(GMAC_BASE + GETH_RX_DESC_LIST),
            nic.recv_ring.as_ptr() as usize as u32,
            "the RX descriptor list register must point at the ring, or the DMA \
             walks whatever was in the register at reset"
        );
        assert_eq!(
            fake::read(GMAC_BASE + GETH_TX_DESC_LIST),
            nic.send_ring.as_ptr() as usize as u32,
            "and the same for TX"
        );
        assert_eq!(
            fake::phy_reg(MII_BMCR) & BMCR_PDOWN,
            0,
            "the PHY must be brought out of power-down"
        );
    }

    #[test]
    fn a_gmac_whose_soft_reset_never_completes_fails_the_bring_up_not_the_boot() {
        let _dev = fake::with_phy(Phy::gigabit_partner()).with_mac_stuck_in_reset();
        let mut nic = driver();

        // `rtlx::init` propagates this error precisely so the board still boots
        // without networking. An `unwrap()` in here panicked first and made that
        // mitigation dead code.
        assert!(
            nic.open().is_err(),
            "a MAC that never leaves soft reset must come back as an error"
        );
    }

    #[test]
    fn a_phy_stuck_in_reset_does_not_wedge_the_bring_up() {
        let _dev = fake::with_phy(Phy::gigabit_partner()).with_phy_stuck_in_reset();
        let mut nic = driver();

        // Bounded busy-waits, not infinite ones: the point is that `open`
        // returns at all.
        nic.open()
            .expect("a PHY that never leaves reset must not stop the MAC coming up");
    }

    // ------------------------------------------------------------- the address

    #[test]
    fn a_mac_the_board_never_programmed_falls_back_to_the_built_in_one() {
        let built_in = RTL8211F::<ProviderImpl>::new(&[0u8; 6]).get_umac();
        assert_ne!(built_in, [0u8; 6], "all zeros is not a usable address");
        assert_eq!(
            built_in[0] & 0x01,
            0,
            "the fallback must not be a multicast address"
        );

        assert_eq!(
            RTL8211F::<ProviderImpl>::new(&[0xff; 6]).get_umac(),
            built_in,
            "broadcast is not a usable address either"
        );
        assert_eq!(
            RTL8211F::<ProviderImpl>::new(&[0x01, 0, 0, 0, 0, 1]).get_umac(),
            built_in,
            "nor is a multicast one"
        );
        assert_eq!(
            RTL8211F::<ProviderImpl>::new(&[0x02, 0x11, 0x22, 0x33, 0x44, 0x55]).get_umac(),
            [0x02, 0x11, 0x22, 0x33, 0x44, 0x55],
            "a usable address is kept as it is"
        );
    }

    #[test]
    fn the_mac_lands_in_the_two_halves_the_gmac_reads_it_from() {
        let _dev = fake::with_phy(Phy::gigabit_partner());
        let nic = driver();
        nic.set_umac(&[0x02, 0x11, 0x22, 0x33, 0x44, 0x55], 0);

        assert_eq!(
            fake::read(GMAC_BASE + GETH_ADDR_HI),
            0x5544,
            "the high half is the last two bytes, low byte first"
        );
        assert_eq!(
            fake::read(GMAC_BASE + GETH_ADDR_LO),
            0x3322_1102,
            "and the low half is the first four, in the same order"
        );

        // Slot 1 lives eight bytes further up, not four.
        nic.set_umac(&[0x06, 0x01, 0x02, 0x03, 0x04, 0x05], 1);
        assert_eq!(fake::read(GMAC_BASE + GETH_ADDR_HI + 8), 0x0504);
        assert_eq!(fake::read(GMAC_BASE + GETH_ADDR_LO + 8), 0x0302_0106);
        assert_eq!(
            fake::read(GMAC_BASE + GETH_ADDR_HI),
            0x5544,
            "and writing slot 1 must not have landed on slot 0"
        );
    }
}
