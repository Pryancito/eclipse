//! Cada offset y cada bit del GENET, clavado contra el valor que publican los
//! drivers libres.
//!
//! **Por que existe este fichero.** Del BCM2711 no hay hoja de datos publica
//! del GENET, y en este proyecto **no hay ninguna Raspberry Pi** con la que
//! comprobar nada (28-oct: «aqui no tengo rpi ni disco para rpi»); QEMU
//! tampoco emula esta pieza, ni en `raspi4b`. Asi que el unico error que se
//! puede cazar aqui no es que el silicio sea distinto, es **haber copiado mal
//! un numero**, que es el error realista de los dos.
//!
//! Cada linea lleva la constante de la que sale, con su nombre tal y como
//! aparece en `drivers/net/bcmgenet.c` de U-Boot (y, donde coinciden, en
//! `drivers/net/ethernet/broadcom/genet/bcmgenet.h` de Linux). Si alguien
//! cambia un offset del driver, este fichero dice contra que lo estaba
//! comparando.

use super::rings::*;
use super::*;

/// Los offsets de los cuatro bloques, tal y como los define U-Boot:
/// `GENET_SYS_OFF 0x0000`, `GENET_EXT_OFF 0x0080`, `GENET_RBUF_OFF 0x0300`,
/// `GENET_UMAC_OFF 0x0800`.
#[test]
fn the_block_offsets_are_the_published_ones() {
    assert_eq!(SYS_REV_CTRL, 0x0000, "SYS_REV_CTRL 0x00");
    assert_eq!(SYS_PORT_CTRL, 0x0004, "SYS_PORT_CTRL 0x04");
    assert_eq!(
        SYS_RBUF_FLUSH_CTRL, 0x0008,
        "SYS_RBUF_FLUSH_CTRL (GENET_SYS_OFF + 0x08)"
    );
    assert_eq!(
        SYS_TBUF_FLUSH_CTRL, 0x000C,
        "SYS_TBUF_FLUSH_CTRL (GENET_SYS_OFF + 0x0c)"
    );
    assert_eq!(
        EXT_RGMII_OOB_CTRL, 0x008C,
        "EXT_RGMII_OOB_CTRL (GENET_EXT_OFF + 0x0c)"
    );
    assert_eq!(RBUF_CTRL, 0x0300, "RBUF_CTRL (GENET_RBUF_OFF + 0x00)");
    assert_eq!(
        RBUF_TBUF_SIZE_CTRL, 0x03B4,
        "RBUF_TBUF_SIZE_CTRL (GENET_RBUF_OFF + 0xb4)"
    );
    assert_eq!(UMAC_CMD, 0x0808, "UMAC_CMD (GENET_UMAC_OFF + 0x008)");
    assert_eq!(UMAC_MAC0, 0x080C, "UMAC_MAC0 (GENET_UMAC_OFF + 0x00c)");
    assert_eq!(UMAC_MAC1, 0x0810, "UMAC_MAC1 (GENET_UMAC_OFF + 0x010)");
    assert_eq!(
        UMAC_MAX_FRAME_LEN, 0x0814,
        "UMAC_MAX_FRAME_LEN (GENET_UMAC_OFF + 0x014)"
    );
    assert_eq!(
        UMAC_TX_FLUSH, 0x0B34,
        "UMAC_TX_FLUSH (GENET_UMAC_OFF + 0x334)"
    );
    assert_eq!(
        UMAC_MIB_CTRL, 0x0D80,
        "UMAC_MIB_CTRL (GENET_UMAC_OFF + 0x580)"
    );
    assert_eq!(UMAC_MDIO_CMD, 0x0E14, "MDIO_CMD (GENET_UMAC_OFF + 0x614)");
}

/// `PORT_MODE_EXT_GPHY 3`, y los bits de `EXT_RGMII_OOB_CTRL`: `RGMII_LINK
/// BIT(4)`, `OOB_DISABLE BIT(5)`, `RGMII_MODE_EN BIT(6)`, `ID_MODE_DIS
/// BIT(16)`.
#[test]
fn the_port_and_rgmii_bits_are_the_published_ones() {
    assert_eq!(PORT_MODE_EXT_GPHY, 3);
    assert_eq!(RGMII_LINK, 1 << 4);
    assert_eq!(OOB_DISABLE, 1 << 5);
    assert_eq!(RGMII_MODE_EN, 1 << 6);
    assert_eq!(ID_MODE_DIS, 1 << 16);
    assert_eq!(RBUF_ALIGN_2B, 1 << 1);
}

/// Los bits de `UMAC_CMD`: `CMD_TX_EN BIT(0)`, `CMD_RX_EN BIT(1)`,
/// `CMD_SPEED_SHIFT 2`, `CMD_SPEED_MASK 3`, `CMD_SW_RESET BIT(13)`,
/// `CMD_LCL_LOOP_EN BIT(15)`; `CMD_PROMISC BIT(4)` y `CMD_HD_EN BIT(10)` son
/// de Linux (U-Boot no los usa).
#[test]
fn the_umac_cmd_bits_are_the_published_ones() {
    assert_eq!(CMD_TX_EN, 1 << 0);
    assert_eq!(CMD_RX_EN, 1 << 1);
    assert_eq!(CMD_SPEED_SHIFT, 2);
    assert_eq!(CMD_SPEED_MASK, 3);
    assert_eq!(CMD_PROMISC, 1 << 4);
    assert_eq!(CMD_HD_EN, 1 << 10);
    assert_eq!(CMD_SW_RESET, 1 << 13);
    assert_eq!(CMD_LCL_LOOP_EN, 1 << 15);
    // Y los cuatro bits que comparten registro no se pisan entre si.
    let todos = [
        CMD_TX_EN,
        CMD_RX_EN,
        CMD_SPEED_MASK << CMD_SPEED_SHIFT,
        CMD_PROMISC,
        CMD_HD_EN,
        CMD_SW_RESET,
        CMD_LCL_LOOP_EN,
    ];
    let mut visto = 0u32;
    for b in todos {
        assert_eq!(visto & b, 0, "{:#x} se solapa con otro bit de UMAC_CMD", b);
        visto |= b;
    }
}

/// `UMAC_SPEED_10 0`, `UMAC_SPEED_100 1`, `UMAC_SPEED_1000 2`,
/// `UMAC_SPEED_2500 3`.
#[test]
fn the_speed_codes_are_the_published_ones() {
    assert_eq!(UMAC_SPEED_10, 0);
    assert_eq!(UMAC_SPEED_100, 1);
    assert_eq!(UMAC_SPEED_1000, 2);
    assert_eq!(UMAC_SPEED_2500, 3);
    // Y los tres que el driver usa caben en el campo de dos bits.
    for s in [LinkSpeed::Mbps10, LinkSpeed::Mbps100, LinkSpeed::Mbps1000] {
        assert_eq!(s.umac_code() & !CMD_SPEED_MASK, 0);
    }
}

/// `MIB_RESET_RX BIT(0)`, `MIB_RESET_RUNT BIT(1)`, `MIB_RESET_TX BIT(2)`.
#[test]
fn the_mib_reset_bits_are_the_published_ones() {
    assert_eq!(MIB_RESET_RX, 1 << 0);
    assert_eq!(MIB_RESET_RUNT, 1 << 1);
    assert_eq!(MIB_RESET_TX, 1 << 2);
}

/// `MDIO_START_BUSY BIT(29)`, `MDIO_READ_FAIL BIT(28)`, `MDIO_RD (2 << 26)`,
/// `MDIO_WR BIT(26)`, `MDIO_PMD_SHIFT 21`, `MDIO_PMD_MASK 0x1f`,
/// `MDIO_REG_SHIFT 16`, `MDIO_REG_MASK 0x1f`.
#[test]
fn the_mdio_bits_are_the_published_ones() {
    assert_eq!(MDIO_START_BUSY, 1 << 29);
    assert_eq!(MDIO_READ_FAIL, 1 << 28);
    assert_eq!(MDIO_RD, 2 << 26);
    assert_eq!(MDIO_WR, 1 << 26);
    assert_eq!(MDIO_PMD_SHIFT, 21);
    assert_eq!(MDIO_PMD_MASK, 0x1F);
    assert_eq!(MDIO_REG_SHIFT, 16);
    assert_eq!(MDIO_REG_MASK, 0x1F);
    // Lectura y escritura son DOS BITS DISTINTOS, no un bit de direccion
    // sobre uno de «es MDIO»: `MDIO_WR` es el 26 y `MDIO_RD` el 27, y no
    // comparten nada. Un `MDIO_RD` escrito como `1 << 26` --que es lo que
    // parece si se lee `2 << 26` como «el bit 26, valor 2»-- seria una
    // escritura con la direccion del PHY como dato.
    assert_eq!(MDIO_WR, 1 << 26);
    assert_eq!(MDIO_RD, 1 << 27);
    assert_eq!(MDIO_RD & MDIO_WR, 0);
    // Los dos campos de 5 bits no se solapan, ni con los bits de control.
    let pmd = MDIO_PMD_MASK << MDIO_PMD_SHIFT;
    let reg = MDIO_REG_MASK << MDIO_REG_SHIFT;
    assert_eq!(pmd & reg, 0);
    assert_eq!(
        pmd & (MDIO_RD | MDIO_WR | MDIO_START_BUSY | MDIO_READ_FAIL),
        0
    );
    assert_eq!(
        reg & (MDIO_RD | MDIO_WR | MDIO_START_BUSY | MDIO_READ_FAIL),
        0
    );
    // Y el dato de 16 bits tampoco pisa el registro ni la direccion.
    assert_eq!(0xFFFFu32 & (pmd | reg), 0);
}

/// `ENET_MAX_MTU_SIZE`, que U-Boot compone como `ETH_DATA_LEN + ETH_HLEN +
/// VLAN_HLEN + ENET_BRCM_TAG_LEN + ETH_FCS_LEN + ENET_PAD` y comenta que son
/// 1528 redondeados a 1536.
#[test]
fn the_frame_ceiling_is_the_published_one() {
    assert_eq!(ENET_MAX_MTU_SIZE, 1536);
    assert_eq!(ENET_MAX_MTU_SIZE % 256, 0, "multiplo de 256");
    // Y da para una trama entera con su FCS, que es lo que de verdad importa.
    assert!(ENET_MAX_MTU_SIZE as usize >= MAX_TX_FRAME + 4);
}

/// Los offsets de los anillos: `GENET_RX_OFF 0x2000`, `GENET_TX_OFF 0x4000`,
/// `TOTAL_DESCS 256`, `DMA_DESC_SIZE 12`, `DEFAULT_Q 0x10`,
/// `DMA_RING_SIZE 0x40`.
#[test]
fn the_ring_offsets_are_the_published_ones() {
    assert_eq!(GENET_RX_OFF, 0x2000);
    assert_eq!(GENET_TX_OFF, 0x4000);
    assert_eq!(TOTAL_DESCS, 256);
    assert_eq!(DMA_DESC_SIZE, 12);
    assert_eq!(DEFAULT_Q, 0x10);
    assert_eq!(DMA_RING_SIZE, 0x40);
    assert_eq!(DMA_RINGS_SIZE, 0x40 * 17);
    assert_eq!(DMA_DESC_LENGTH_STATUS, 0x00);
    assert_eq!(DMA_DESC_ADDRESS_LO, 0x04);
    assert_eq!(DMA_DESC_ADDRESS_HI, 0x08);
    // Y los valores derivados, por si alguien toca una de las piezas.
    assert_eq!(GENET_RDMA_REG_OFF, 0x2C00);
    assert_eq!(GENET_TDMA_REG_OFF, 0x4C00);
    assert_eq!(RDMA_RING_REG_BASE, 0x3000);
    assert_eq!(TDMA_RING_REG_BASE, 0x5000);
    assert_eq!(RDMA_REG_BASE, 0x3040);
    assert_eq!(TDMA_REG_BASE, 0x5040);
}

/// Los registros de dentro de una cola: `TDMA_READ_PTR +0x00`,
/// `TDMA_CONS_INDEX +0x08`, `TDMA_PROD_INDEX +0x0c`, `DMA_RING_BUF_SIZE 0x10`,
/// `DMA_START_ADDR 0x14`, `DMA_END_ADDR 0x1c`, `DMA_MBUF_DONE_THRESH 0x24`,
/// `TDMA_FLOW_PERIOD +0x28`, `TDMA_WRITE_PTR +0x2c`; y en recepcion
/// `RDMA_WRITE_PTR +0x00`, `RDMA_PROD_INDEX +0x08`, `RDMA_CONS_INDEX +0x0c`,
/// `RDMA_XON_XOFF_THRESH +0x28`, `RDMA_READ_PTR +0x2c`.
#[test]
fn the_per_queue_registers_are_the_published_ones() {
    assert_eq!(DMA_RING_WRITE_PTR, 0x00);
    assert_eq!(DMA_RING_BUF_SIZE, 0x10);
    assert_eq!(DMA_START_ADDR, 0x14);
    assert_eq!(DMA_END_ADDR, 0x1C);
    assert_eq!(DMA_MBUF_DONE_THRESH, 0x24);
    assert_eq!(DMA_RING_READ_PTR, 0x2C);
    assert_eq!(RDMA_PROD_INDEX - RDMA_RING_REG_BASE, 0x08);
    assert_eq!(RDMA_CONS_INDEX - RDMA_RING_REG_BASE, 0x0C);
    assert_eq!(RDMA_XON_XOFF_THRESH - RDMA_RING_REG_BASE, 0x28);
    assert_eq!(TDMA_CONS_INDEX - TDMA_RING_REG_BASE, 0x08);
    assert_eq!(TDMA_PROD_INDEX - TDMA_RING_REG_BASE, 0x0C);
    assert_eq!(TDMA_FLOW_PERIOD - TDMA_RING_REG_BASE, 0x28);
    // `WRITE_PTR` y `READ_PTR` comparten offset entre rx y tx, pero el que es
    // del MAC es el contrario en cada uno: el del driver nunca es el mismo que
    // el indice que el driver escribe.
    assert_ne!(DMA_RING_WRITE_PTR, DMA_RING_READ_PTR);
}

/// Los registros del DMA entero: `DMA_RING_CFG 0x00`, `DMA_CTRL 0x04`,
/// `DMA_SCB_BURST_SIZE 0x0c`, `DMA_EN BIT(0)`,
/// `DMA_RING_BUF_EN_SHIFT 0x01`, `DMA_MAX_BURST_LENGTH 0x8`.
#[test]
fn the_dma_control_registers_are_the_published_ones() {
    assert_eq!(DMA_RING_CFG, 0x00);
    assert_eq!(DMA_CTRL, 0x04);
    assert_eq!(DMA_SCB_BURST_SIZE, 0x0C);
    assert_eq!(DMA_EN, 1);
    assert_eq!(DMA_RING_BUF_EN_SHIFT, 1);
    assert_eq!(DMA_MAX_BURST_LENGTH, 0x8);
    // La mascara de la cola por defecto no pisa el bit de encendido: con
    // `DMA_RING_BUF_EN_SHIFT` a cero, encender la cola 0 seria encender el DMA
    // y al reves.
    assert_eq!(
        (1u32 << (DEFAULT_Q as u32 + DMA_RING_BUF_EN_SHIFT)) & DMA_EN,
        0
    );
}

/// Los bits del estado de un descriptor: `DMA_BUFLENGTH_MASK 0x0fff`,
/// `DMA_BUFLENGTH_SHIFT 16`, `DMA_RING_SIZE_SHIFT 16`, `DMA_OWN 0x8000`,
/// `DMA_EOP 0x4000`, `DMA_SOP 0x2000`, `DMA_TX_APPEND_CRC 0x0040`,
/// `DMA_TX_QTAG_SHIFT 7`.
#[test]
fn the_descriptor_status_bits_are_the_published_ones() {
    assert_eq!(DMA_BUFLENGTH_MASK, 0x0FFF);
    assert_eq!(DMA_BUFLENGTH_SHIFT, 16);
    assert_eq!(DMA_RING_SIZE_SHIFT, 16);
    assert_eq!(DMA_OWN, 0x8000);
    assert_eq!(DMA_EOP, 0x4000);
    assert_eq!(DMA_SOP, 0x2000);
    assert_eq!(DMA_TX_APPEND_CRC, 0x0040);
    assert_eq!(DMA_TX_QTAG_SHIFT, 7);
    assert_eq!(DMA_TX_QTAG_ALL, 0x3F);
    // La longitud va arriba y el estado abajo: no se pisan.
    let longitud = DMA_BUFLENGTH_MASK << DMA_BUFLENGTH_SHIFT;
    let estado =
        DMA_OWN | DMA_EOP | DMA_SOP | DMA_TX_APPEND_CRC | (DMA_TX_QTAG_ALL << DMA_TX_QTAG_SHIFT);
    assert_eq!(longitud & estado, 0);
    // Y el buffer de 2 KiB cabe en el campo de 12 bits que lo declara.
    assert_eq!(
        (RX_BUF_LENGTH as u32) & !DMA_BUFLENGTH_MASK,
        0,
        "2048 no cabe en 12 bits si la mascara esta mal"
    );
}

/// `RX_BUF_LENGTH 2048` y `RX_BUF_OFFSET 2`.
#[test]
fn the_receive_buffer_shape_is_the_published_one() {
    assert_eq!(RX_BUF_LENGTH, 2048);
    assert_eq!(RX_BUF_OFFSET, 2);
    // El hueco tiene que dar para la trama mas larga que el UMAC acepta.
    assert!(RX_BUF_LENGTH >= ENET_MAX_MTU_SIZE as usize + RX_BUF_OFFSET);
}

/// El identificador del PHY de la Pi 4. Linux casa el BCM54210E con
/// `0x600d84a0` y mascara `0xfffffff0`; el BCM54213PE de la Pi 4 es de esa
/// misma familia.
#[test]
fn the_phy_id_and_its_mask_are_the_published_ones() {
    assert_eq!(BCM5421X_PHY_ID, 0x600D_84A0);
    assert_eq!(BCM5421X_PHY_ID_MASK, 0xFFFF_FFF0);
    // La mascara tapa exactamente el nibble de la revision.
    assert_eq!(!BCM5421X_PHY_ID_MASK, 0xF);
    assert_eq!(BCM5421X_PHY_ID & !BCM5421X_PHY_ID_MASK, 0);
}
