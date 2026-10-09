use super::*;

/// Un descriptor de configuración sintético: cabecera, luego los
/// descriptores que se le pasen ya montados.
fn config(parts: &[&[u8]]) -> Vec<u8> {
    let mut out = alloc::vec![9u8, 0x02, 0, 0, 1, 1, 0, 0x80, 50];
    for p in parts {
        out.extend_from_slice(p);
    }
    let total = out.len() as u16;
    out[2] = total as u8;
    out[3] = (total >> 8) as u8;
    out
}

fn iface(num: u8, alt: u8, class: u8, sub: u8, proto: u8) -> Vec<u8> {
    alloc::vec![9, USB_DESC_IFACE, num, alt, 1, class, sub, proto, 0]
}

fn endpoint(addr: u8, attr: u8, mps: u16, interval: u8) -> Vec<u8> {
    alloc::vec![
        7,
        USB_DESC_EP,
        addr,
        attr,
        mps as u8,
        (mps >> 8) as u8,
        interval
    ]
}

#[test]
fn every_interface_is_recorded_and_not_only_the_ones_with_a_driver() {
    // Lo que habia que poder ver: un pendrive, cuya unica interfaz es de
    // una clase que este driver no maneja.
    let raw = config(&[&iface(0, 0, 0x08, 0x06, 0x50)]);
    let (ifaces, dropped) = config_interfaces(&raw);
    assert_eq!(
        ifaces,
        alloc::vec![IfaceRecord {
            num: 0,
            class: 0x08,
            subclass: 0x06,
            proto: 0x50
        }]
    );
    assert_eq!(dropped, 0);
}

#[test]
fn an_alternate_setting_is_the_same_interface_and_not_another_one() {
    // Un hub multi-TT declara su interfaz dos veces, alt 0 y alt 1.
    // Contarlas por separado llena la lista de duplicados.
    let raw = config(&[
        &iface(0, 0, USB_CLASS_HUB, 0, 0),
        &iface(0, 1, USB_CLASS_HUB, 0, 2),
    ]);
    let (ifaces, _) = config_interfaces(&raw);
    assert_eq!(ifaces.len(), 1);
    assert_eq!(ifaces[0].proto, 0);
}

#[test]
fn more_interfaces_than_fit_are_counted_and_not_dropped_in_silence() {
    let mut parts: Vec<Vec<u8>> = Vec::new();
    for n in 0..(MAX_IFACES_RECORDED as u8 + 3) {
        parts.push(iface(n, 0, USB_CLASS_HID, 0, 1));
    }
    let refs: Vec<&[u8]> = parts.iter().map(|p| p.as_slice()).collect();
    let (ifaces, dropped) = config_interfaces(&config(&refs));
    assert_eq!(ifaces.len(), MAX_IFACES_RECORDED);
    assert_eq!(dropped, 3);
}

#[test]
fn the_hub_status_endpoint_is_the_interrupt_in_of_the_hub_interface() {
    let raw = config(&[
        &iface(0, 0, USB_CLASS_HUB, 0, 0),
        &endpoint(0x81, 0x03, 2, 12),
    ]);
    assert_eq!(
        class_int_in_endpoint(&raw, USB_CLASS_HUB),
        Some((0x81, 2, 12))
    );
}

#[test]
fn an_endpoint_of_another_interface_is_not_the_hubs() {
    // Un compuesto raro: una interfaz HID con su endpoint delante del hub.
    // Quedarse con el primer interrupt-IN del descriptor habria armado el
    // endpoint del raton como si fuera el del hub.
    let raw = config(&[
        &iface(0, 0, USB_CLASS_HID, 1, 1),
        &endpoint(0x81, 0x03, 8, 10),
        &iface(1, 0, USB_CLASS_HUB, 0, 0),
        &endpoint(0x82, 0x03, 1, 12),
    ]);
    assert_eq!(
        class_int_in_endpoint(&raw, USB_CLASS_HUB),
        Some((0x82, 1, 12))
    );
    assert_eq!(
        class_int_in_endpoint(&raw, USB_CLASS_HID),
        Some((0x81, 8, 10))
    );
    assert_eq!(class_int_in_endpoint(&raw, 0x08), None);
}

#[test]
fn an_out_or_bulk_endpoint_is_not_an_interrupt_in() {
    let raw = config(&[
        &iface(0, 0, USB_CLASS_HUB, 0, 0),
        // Interrupcion, pero OUT.
        &endpoint(0x01, 0x03, 2, 12),
        // IN, pero bulk.
        &endpoint(0x82, 0x02, 512, 0),
    ]);
    assert_eq!(class_int_in_endpoint(&raw, USB_CLASS_HUB), None);
}

#[test]
fn the_high_speed_transaction_bits_never_leak_into_the_packet_size() {
    // wMaxPacketSize = 0x1400: 1024 bytes y dos transacciones adicionales
    // en los bits 12:11. Esos bits en el Max Packet Size del contexto son
    // un tamano absurdo.
    let raw = config(&[
        &iface(0, 0, USB_CLASS_HUB, 0, 0),
        &endpoint(0x81, 0x03, 0x1400, 4),
    ]);
    assert_eq!(
        class_int_in_endpoint(&raw, USB_CLASS_HUB).map(|e| e.1),
        Some(0x400)
    );
}

#[test]
fn a_descriptor_that_lies_about_its_length_stops_the_walk_instead_of_hanging() {
    // bLength 0 avanzaria cero bytes y daria vueltas para siempre; uno que
    // se sale del buffer leeria mas alla de lo que el dispositivo mando.
    assert_eq!(config_descriptors(&[0, USB_DESC_IFACE, 0]).count(), 0);
    assert_eq!(config_descriptors(&[1, USB_DESC_IFACE]).count(), 0);
    assert_eq!(config_descriptors(&[40, USB_DESC_IFACE, 0, 0]).count(), 0);
    // Y lo que si cabe se entrega antes de parar en lo que no.
    let raw = alloc::vec![4u8, USB_DESC_IFACE, 0, 0, 40, USB_DESC_EP, 0];
    assert_eq!(config_descriptors(&raw).count(), 1);
}

#[test]
fn a_dci_is_two_n_for_an_out_endpoint_and_two_n_plus_one_for_an_in_one() {
    // Y es la inversa exacta de `ep_addr_from_dci`: si las dos no coinciden,
    // un CLEAR_FEATURE acaba dirigido a otro endpoint.
    for num in 1..16u8 {
        for &dir in &[0u8, 0x80] {
            let addr = num | dir;
            let dci = dci_from_ep_addr(addr).expect("cabe en 31 endpoints");
            assert_eq!(dci, num * 2 + u8::from(dir != 0));
            assert_eq!(ep_addr_from_dci(dci), addr as u16);
        }
    }
    // EP0 no tiene direccion: su DCI lo pone `setup_device`.
    assert_eq!(dci_from_ep_addr(0x00), None);
    assert_eq!(dci_from_ep_addr(0x80), None);
}

#[test]
fn a_command_block_wrapper_carries_its_tag_length_direction_and_command() {
    let w = bot_cbw(0x1234_5678, 36, true, 0, &[SCSI_INQUIRY, 0, 0, 0, 36, 0]).expect("CBW valido");
    assert_eq!(&w[0..4], b"USBC");
    assert_eq!(u32::from_le_bytes([w[4], w[5], w[6], w[7]]), 0x1234_5678);
    assert_eq!(u32::from_le_bytes([w[8], w[9], w[10], w[11]]), 36);
    assert_eq!(w[12], 0x80, "bmCBWFlags: solo el bit 7, la direccion");
    assert_eq!(w[13], 0);
    assert_eq!(w[14], 6);
    assert_eq!(&w[15..21], &[SCSI_INQUIRY, 0, 0, 0, 36, 0]);
    // Lo que no se llena queda a cero: el hueco del bloque de orden son 16
    // bytes y el dispositivo lee bCBWCBLength de ellos.
    assert!(w[21..].iter().all(|&b| b == 0));
}

#[test]
fn a_write_command_is_not_marked_as_an_in_transfer() {
    let w = bot_cbw(1, 512, false, 3, &[0x2a, 0, 0, 0, 0, 0, 0, 0, 1, 0]).unwrap();
    assert_eq!(w[12], 0x00);
    assert_eq!(w[13], 3, "el LUN va en bCBWLUN");
}

#[test]
fn a_command_that_does_not_fit_the_wrapper_is_refused() {
    // El hueco del bloque de orden son 16 bytes, ni uno mas; y una orden
    // vacia no es una orden.
    assert!(bot_cbw(1, 0, false, 0, &[0u8; 16]).is_some());
    assert!(bot_cbw(1, 0, false, 0, &[0u8; 17]).is_none());
    assert!(bot_cbw(1, 0, false, 0, &[]).is_none());
}

fn csw(sig: &[u8; 4], tag: u32, residue: u32, status: u8) -> [u8; BOT_CSW_LEN] {
    let mut w = [0u8; BOT_CSW_LEN];
    w[0..4].copy_from_slice(sig);
    w[4..8].copy_from_slice(&tag.to_le_bytes());
    w[8..12].copy_from_slice(&residue.to_le_bytes());
    w[12] = status;
    w
}

#[test]
fn a_status_wrapper_is_read_when_it_answers_the_command_that_was_sent() {
    let raw = csw(b"USBS", 7, 4, 1);
    assert_eq!(
        bot_parse_csw(&raw, 7),
        Some(BotCsw {
            tag: 7,
            residue: 4,
            status: 1
        })
    );
}

#[test]
fn a_status_wrapper_of_another_command_is_not_this_commands_answer() {
    // Es el fallo que convierte el resultado de la orden anterior en el de
    // esta: un «bien» que era de otra pregunta.
    let raw = csw(b"USBS", 6, 0, 0);
    assert_eq!(bot_parse_csw(&raw, 7), None);
}

#[test]
fn a_status_wrapper_that_is_not_one_is_refused() {
    // Firma mala, estado que no existe, y uno truncado.
    assert_eq!(bot_parse_csw(&csw(b"USBC", 1, 0, 0), 1), None);
    assert_eq!(bot_parse_csw(&csw(b"USBS", 1, 0, 3), 1), None);
    assert_eq!(bot_parse_csw(&csw(b"USBS", 1, 0, 0x80), 1), None);
    assert_eq!(bot_parse_csw(&csw(b"USBS", 1, 0, 0)[..12], 1), None);
    // Y el error de fase (2) si es un estado: pide un reset del transporte.
    assert_eq!(
        bot_parse_csw(&csw(b"USBS", 1, 0, 2), 1).map(|c| c.status),
        Some(2)
    );
}

fn inquiry_bytes(dev_type: u8, rmb: u8, vendor: &[u8], product: &[u8]) -> [u8; 36] {
    let mut raw = [b' '; 36];
    raw[0] = dev_type;
    raw[1] = rmb;
    raw[2] = 0x06;
    raw[3] = 0x02;
    raw[4] = 31;
    raw[5] = 0;
    raw[6] = 0;
    raw[7] = 0;
    raw[8..8 + vendor.len()].copy_from_slice(vendor);
    raw[16..16 + product.len()].copy_from_slice(product);
    raw[32..36].copy_from_slice(b"1.00");
    raw
}

#[test]
fn an_inquiry_gives_the_type_the_removable_bit_and_the_three_text_fields() {
    let raw = inquiry_bytes(0x00, 0x80, b"SanDisk", b"Ultra");
    let inq = scsi_parse_inquiry(&raw).expect("INQUIRY valido");
    assert_eq!(inq.dev_type, 0x00, "0 es un disco de bloques");
    assert!(inq.removable);
    // SCSI rellena con espacios, no con ceros: volcarlo tal cual deja una
    // columna de huecos en cada linea de /proc.
    assert_eq!(scsi_text(&inq.vendor), "SanDisk");
    assert_eq!(scsi_text(&inq.product), "Ultra");
    assert_eq!(scsi_text(&inq.revision), "1.00");
}

#[test]
fn the_device_type_is_five_bits_and_the_removable_bit_is_only_bit_seven() {
    // Byte 0 lleva el qualifier en los bits 7:5; tomarlo por el tipo
    // convierte un disco normal en un dispositivo que no existe.
    let raw = inquiry_bytes(0xe0 | 0x05, 0x7f, b"HL-DT-ST", b"DVDRAM");
    let inq = scsi_parse_inquiry(&raw).unwrap();
    assert_eq!(inq.dev_type, 0x05, "5 es un CD/DVD");
    assert!(
        !inq.removable,
        "RMB es solo el bit 7; los otros siete son reservados"
    );
}

#[test]
fn a_text_field_with_control_bytes_does_not_break_the_line_it_is_printed_on() {
    assert_eq!(scsi_text(b"ab\x00cd\x1b  "), "ab.cd.");
    assert_eq!(scsi_text(b"        "), "");
}

#[test]
fn an_inquiry_shorter_than_its_mandatory_fields_is_refused() {
    assert_eq!(scsi_parse_inquiry(&[0u8; 35]), None);
    assert_eq!(scsi_parse_inquiry(&[]), None);
}

#[test]
fn a_read_capacity_is_big_endian_and_names_the_last_block_not_the_count() {
    // 0x0000_0fff bloques de 512: el ultimo LBA es 4095, o sea 4096
    // bloques. Tomar el ultimo LBA por la cuenta deja el ultimo sector
    // fuera del disco.
    let raw = [0x00, 0x00, 0x0f, 0xff, 0x00, 0x00, 0x02, 0x00];
    let cap = scsi_parse_capacity10(&raw).expect("capacidad valida");
    assert_eq!(cap.last_lba, 4095);
    assert_eq!(cap.block_size, 512);
    assert!(!cap.needs_16);
    assert_eq!(scsi_sectors_512(&cap), 4096);
}

#[test]
fn a_four_kilobyte_block_counts_as_eight_sectors_of_five_hundred_twelve() {
    let raw = [0x00, 0x00, 0x00, 0x09, 0x00, 0x00, 0x10, 0x00];
    let cap = scsi_parse_capacity10(&raw).unwrap();
    assert_eq!(cap.block_size, 4096);
    // 10 bloques de 4 KiB = 80 sectores de 512 B.
    assert_eq!(scsi_sectors_512(&cap), 80);
}

#[test]
fn a_disk_that_does_not_fit_in_thirty_two_bits_asks_for_the_sixteen_byte_command() {
    let raw = [0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x02, 0x00];
    let cap = scsi_parse_capacity10(&raw).unwrap();
    assert!(
        cap.needs_16,
        "0xffffffff no es una capacidad: es «preguntame con el de 16»"
    );
    // Y el de 16 trae el LBA en ocho bytes.
    let raw16 = [
        0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00,
    ];
    let big = scsi_parse_capacity16(&raw16).expect("capacidad de 16 valida");
    assert_eq!(big.last_lba, 0x1_0000_0000);
    assert_eq!(big.block_size, 512);
    assert!(!big.needs_16);
}

#[test]
fn a_block_size_of_zero_is_not_a_capacity() {
    // Dividir 512 entre el seria una division por cero, y creerselo seria
    // un disco de capacidad infinita.
    assert_eq!(scsi_parse_capacity10(&[0, 0, 0, 9, 0, 0, 0, 0]), None);
    assert_eq!(scsi_parse_capacity16(&[0u8; 12]), None);
    assert_eq!(scsi_parse_capacity10(&[0, 0, 0, 9, 0, 0, 2]), None);
}

#[test]
fn the_bulk_pair_belongs_to_its_own_interface() {
    // Un disco externo con lector de tarjetas declara dos interfaces de
    // almacenamiento. Coger «el primer bulk del descriptor» para las dos es
    // hablarle al disco equivocado.
    let raw = config(&[
        &iface(0, 0, USB_CLASS_MASS_STORAGE, MSC_SUBCLASS_SCSI, 0x50),
        &endpoint(0x81, 0x02, 512, 0),
        &endpoint(0x02, 0x02, 512, 0),
        &iface(1, 0, USB_CLASS_MASS_STORAGE, MSC_SUBCLASS_SCSI, 0x50),
        &endpoint(0x83, 0x02, 512, 0),
        &endpoint(0x04, 0x02, 512, 0),
    ]);
    assert_eq!(
        iface_bulk_endpoints(&raw, 0),
        Some(((0x81, 512), (0x02, 512)))
    );
    assert_eq!(
        iface_bulk_endpoints(&raw, 1),
        Some(((0x83, 512), (0x04, 512)))
    );
    assert_eq!(iface_bulk_endpoints(&raw, 2), None);
}

#[test]
fn an_interface_missing_half_the_pair_has_no_pair() {
    // Sin el OUT no hay por donde mandar el CBW, asi que media pareja no
    // sirve de nada y armarla a medias deja el endpoint IN configurado para
    // siempre sin nadie que lo use.
    let only_in = config(&[
        &iface(0, 0, USB_CLASS_MASS_STORAGE, MSC_SUBCLASS_SCSI, 0x50),
        &endpoint(0x81, 0x02, 512, 0),
    ]);
    assert_eq!(iface_bulk_endpoints(&only_in, 0), None);
    // Y un interrupt o un isocrono no son bulk.
    let not_bulk = config(&[
        &iface(0, 0, USB_CLASS_MASS_STORAGE, MSC_SUBCLASS_SCSI, 0x50),
        &endpoint(0x81, 0x03, 512, 1),
        &endpoint(0x02, 0x01, 512, 1),
    ]);
    assert_eq!(iface_bulk_endpoints(&not_bulk, 0), None);
}

#[test]
fn the_endpoints_of_an_alternate_setting_are_not_the_selected_ones() {
    let raw = config(&[
        &iface(0, 0, USB_CLASS_MASS_STORAGE, MSC_SUBCLASS_SCSI, 0x50),
        &endpoint(0x81, 0x02, 512, 0),
        &endpoint(0x02, 0x02, 512, 0),
        &iface(0, 1, USB_CLASS_MASS_STORAGE, MSC_SUBCLASS_SCSI, 0x50),
        &endpoint(0x85, 0x02, 1024, 0),
        &endpoint(0x06, 0x02, 1024, 0),
    ]);
    assert_eq!(
        iface_bulk_endpoints(&raw, 0),
        Some(((0x81, 512), (0x02, 512))),
        "los endpoints que siguen a la alternativa 1 son de la alternativa 1"
    );
}

#[test]
fn the_classes_that_matter_have_a_name_and_the_rest_do_not_pretend_to() {
    assert_eq!(usb_class_name(USB_CLASS_HID), "hid");
    assert_eq!(usb_class_name(USB_CLASS_HUB), "hub");
    assert_eq!(usb_class_name(0x08), "almacenamiento");
    assert_eq!(usb_class_name(0x42), "?");
}

/// El Slot Context DW0 tal como lo escribe `setup_device`, para poder
/// comprobar aquí lo que llega al controlador sin un controlador.
fn slot_dw0(topo: &DevTopo, speed: u8) -> u32 {
    (topo.route & 0x000f_ffff)
        | ((speed as u32) << 20)
        | (if topo.tt_multi { 1u32 << 25 } else { 0 })
        | (1u32 << 27)
}

#[test]
fn a_device_on_a_root_port_has_no_route_and_no_translator() {
    let t = DevTopo::root(7);
    assert_eq!(t.route, 0);
    assert_eq!(t.depth, 0);
    assert_eq!(t.root_port, 7);
    assert_eq!((t.parent_slot, t.parent_port), (0, 0));
    assert_eq!((t.tt_slot, t.tt_port), (0, 0));
    // DW0 de un dispositivo HS en el puerto raíz: solo velocidad y
    // Context Entries = 1.
    assert_eq!(slot_dw0(&t, SPEED_HIGH), (3 << 20) | (1 << 27));
}

#[test]
fn each_hub_tier_owns_its_own_nibble_of_the_route_string() {
    let root = DevTopo::root(2);
    // Nivel 1: puerto 3 del hub raíz.
    let t1 = root
        .child(1, SPEED_HIGH, 3, SPEED_HIGH, false)
        .expect("nivel 1");
    assert_eq!(t1.route, 0x3);
    assert_eq!(t1.depth, 1);
    // Nivel 2: puerto 5 de ese hub.
    let t2 = t1
        .child(2, SPEED_HIGH, 5, SPEED_HIGH, false)
        .expect("nivel 2");
    assert_eq!(t2.route, 0x53);
    // Nivel 3: puerto 15, el más alto que cabe en un nibble.
    let t3 = t2
        .child(3, SPEED_HIGH, 15, SPEED_HIGH, false)
        .expect("nivel 3");
    assert_eq!(t3.route, 0xf53);
    // El puerto raíz se arrastra intacto por toda la cadena: es lo que va
    // al Slot Context DW1, no el puerto del hub.
    assert_eq!(t3.root_port, 2);
    assert_eq!(t3.depth, 3);
    // Y el route string vive en los 20 bits bajos de DW0, sin pisar la
    // velocidad.
    assert_eq!(
        slot_dw0(&t3, SPEED_HIGH),
        0xf53 | (3 << 20) | (1 << 27),
        "el route string no puede desbordar al campo Speed"
    );
}

#[test]
fn the_fifth_tier_is_the_last_one_a_route_string_can_name() {
    let mut t = DevTopo::root(1);
    for tier in 1..=USB_MAX_TIERS {
        t = t
            .child(tier, SPEED_HIGH, 1, SPEED_HIGH, false)
            .unwrap_or_else(|| panic!("el nivel {} debería caber", tier));
        assert_eq!(t.depth, tier);
    }
    // Cinco niveles ocupan los 20 bits enteros; el sexto no tiene nibble.
    assert_eq!(t.route, 0x11111);
    assert!(
        t.child(6, SPEED_HIGH, 1, SPEED_HIGH, false).is_none(),
        "un sexto nivel no cabe en el route string y no debe enumerarse"
    );
}

#[test]
fn a_port_that_does_not_fit_a_nibble_is_refused() {
    let root = DevTopo::root(1);
    assert!(root.child(1, SPEED_HIGH, 0, SPEED_HIGH, false).is_none());
    assert!(
        root.child(1, SPEED_HIGH, HUB_MAX_PORTS + 1, SPEED_HIGH, false)
            .is_none(),
        "el puerto 16 se solaparía con el nibble del nivel siguiente"
    );
    assert!(root
        .child(1, SPEED_HIGH, HUB_MAX_PORTS, SPEED_HIGH, false)
        .is_some());
}

#[test]
fn a_low_speed_device_behind_a_high_speed_hub_gets_that_hub_as_its_translator() {
    let hub = DevTopo::root(4);
    let mouse = hub
        .child(9, SPEED_HIGH, 2, SPEED_LOW, false)
        .expect("un ratón LS detrás de un hub HS");
    assert_eq!(
        (mouse.tt_slot, mouse.tt_port),
        (9, 2),
        "el TT es el hub HS del que cuelga, por el puerto del que cuelga"
    );
    assert!(!mouse.tt_multi);
    // Y el mismo hub, con un dispositivo HS, no traduce nada.
    let disk = hub
        .child(9, SPEED_HIGH, 3, SPEED_HIGH, false)
        .expect("un dispositivo HS");
    assert_eq!((disk.tt_slot, disk.tt_port), (0, 0));
}

#[test]
fn a_full_speed_hub_below_a_high_speed_one_keeps_pointing_at_the_translator() {
    // HS raíz -> hub HS (slot 9, puerto 2) -> hub FS -> teclado LS.
    let root = DevTopo::root(1);
    let fs_hub = root
        .child(9, SPEED_HIGH, 2, SPEED_FULL, false)
        .expect("hub FS");
    assert_eq!((fs_hub.tt_slot, fs_hub.tt_port), (9, 2));
    let kbd = fs_hub
        .child(11, SPEED_FULL, 4, SPEED_LOW, false)
        .expect("teclado LS");
    assert_eq!(
        (kbd.tt_slot, kbd.tt_port),
        (9, 2),
        "el traductor sigue siendo el hub HS, no el hub FS intermedio"
    );
    assert_eq!(kbd.route, 0x42);
}

#[test]
fn a_multi_tt_hub_marks_mtt_on_the_child_and_not_on_a_fast_one() {
    let root = DevTopo::root(1);
    let slow = root
        .child(6, SPEED_HIGH, 1, SPEED_FULL, true)
        .expect("FS detrás de un hub multi-TT");
    assert!(slow.tt_multi);
    assert_eq!(slot_dw0(&slow, SPEED_FULL) & (1 << 25), 1 << 25);
    let fast = root
        .child(6, SPEED_HIGH, 1, SPEED_HIGH, true)
        .expect("HS detrás del mismo hub");
    assert!(
        !fast.tt_multi,
        "MTT solo aplica a lo que pasa por el traductor"
    );
    assert_eq!(slot_dw0(&fast, SPEED_HIGH) & (1 << 25), 0);
}

#[test]
fn a_superspeed_hub_has_only_superspeed_children() {
    // Un hub SS no mira los bits de LS/HS: lo que cuelga de él es SS por
    // definición, y los USB 2.0 del mismo conector van por el hub
    // acompañante.
    assert_eq!(hub_port_speed(SPEED_SUPER, HUB_PORT_LOW_SPEED), SPEED_SUPER);
    assert_eq!(hub_port_speed(5, HUB_PORT_HIGH_SPEED), 5);
}

#[test]
fn a_usb2_hub_reads_the_speed_off_its_port_status() {
    assert_eq!(
        hub_port_speed(SPEED_HIGH, HUB_PORT_CONNECTION | HUB_PORT_LOW_SPEED),
        SPEED_LOW
    );
    assert_eq!(
        hub_port_speed(SPEED_HIGH, HUB_PORT_CONNECTION | HUB_PORT_HIGH_SPEED),
        SPEED_HIGH
    );
    // Ninguno de los dos bits = plena velocidad. Es el caso por defecto, y
    // tomarlo por LS dejaría a cada dispositivo FS con un EP0 de 8 bytes.
    assert_eq!(
        hub_port_speed(SPEED_HIGH, HUB_PORT_CONNECTION | HUB_PORT_ENABLE),
        SPEED_FULL
    );
}

#[test]
fn a_hub_descriptor_gives_its_ports_think_time_and_power_delay() {
    // bLength, bDescriptorType, bNbrPorts, wHubCharacteristics,
    // bPwrOn2PwrGood (en unidades de 2 ms).
    let raw = [9u8, USB_DESC_HUB, 4, 0x29, 0x00, 50, 0, 0, 0xff];
    let info = parse_hub_descriptor(&raw).expect("descriptor válido");
    assert_eq!(info.ports, 4);
    // wHubCharacteristics = 0x0029: TT Think Time en los bits 6:5 = 1.
    assert_eq!(info.think_time, 1);
    assert_eq!(info.power_good_ms, 100);
    assert!(!info.multi_tt);
}

#[test]
fn the_power_delay_never_drops_below_the_hundred_milliseconds_of_the_spec() {
    let raw = [9u8, USB_DESC_HUB, 2, 0x00, 0x00, 1, 0, 0, 0];
    let info = parse_hub_descriptor(&raw).expect("descriptor válido");
    assert_eq!(
        info.power_good_ms, 100,
        "2 ms no bastan: un puerto recién encendido tiene 100 ms para contestar"
    );
    let slow = [9u8, USB_DESC_HUB, 2, 0x00, 0x00, 100, 0, 0, 0];
    assert_eq!(
        parse_hub_descriptor(&slow).unwrap().power_good_ms,
        200,
        "y un hub que pide más se respeta"
    );
}

#[test]
fn a_superspeed_hub_descriptor_is_read_with_the_same_first_six_bytes() {
    let raw = [12u8, USB_DESC_SS_HUB, 4, 0x00, 0x00, 10, 0, 0, 0, 0, 0, 0];
    let info = parse_hub_descriptor(&raw).expect("descriptor SS válido");
    assert_eq!(info.ports, 4);
    assert_eq!(info.power_good_ms, 100);
}

#[test]
fn a_descriptor_that_is_not_one_is_refused_instead_of_believed() {
    // Un hub sin puertos, un tipo que no es de hub, uno truncado y uno
    // cuyo bLength no llega a los campos que se leen: ninguno vale, y
    // creerse cualquiera de ellos sería encender puertos que no existen.
    assert!(parse_hub_descriptor(&[9, USB_DESC_HUB, 0, 0, 0, 10]).is_none());
    assert!(parse_hub_descriptor(&[9, 0x02, 4, 0, 0, 10]).is_none());
    assert!(parse_hub_descriptor(&[9, USB_DESC_HUB, 4, 0]).is_none());
    assert!(parse_hub_descriptor(&[3, USB_DESC_HUB, 4, 0, 0, 10]).is_none());
    assert!(parse_hub_descriptor(&[]).is_none());
}

#[test]
fn the_port_count_is_clamped_to_what_a_route_string_can_name() {
    let raw = [9u8, USB_DESC_HUB, 40, 0, 0, 10];
    assert_eq!(
        parse_hub_descriptor(&raw).unwrap().ports,
        HUB_MAX_PORTS,
        "un puerto que no cabe en un nibble no se puede enumerar, así que \
         tampoco se recorre"
    );
}

#[test]
fn the_change_bitmap_is_one_bit_per_port_plus_the_hubs_own() {
    // Siete puertos + el bit del hub = 8 bits = 1 byte. Ocho puertos ya
    // necesitan dos, y pedir uno habria dejado al puerto 8 fuera del
    // informe para siempre.
    assert_eq!(hub_change_bytes(1), 1);
    assert_eq!(hub_change_bytes(7), 1);
    assert_eq!(hub_change_bytes(8), 2);
    assert_eq!(hub_change_bytes(HUB_MAX_PORTS), 2);
}

#[test]
fn the_bit_of_each_port_is_the_port_itself_and_bit_zero_is_not_a_port() {
    // bit 1 -> puerto 1, bit 3 -> puerto 3.
    assert_eq!(hub_changed_ports(&[0b0000_1010], 7), alloc::vec![1, 3]);
    // El bit 0 es un cambio del hub entero, no un puerto: creerselo
    // mandaria un GET_STATUS al puerto 0, que no existe.
    assert_eq!(hub_changed_ports(&[0b0000_0001], 7), alloc::vec![]);
    // Y el puerto 8 vive en el segundo byte, bit 0.
    assert_eq!(hub_changed_ports(&[0, 0b0000_0001], 8), alloc::vec![8]);
}

#[test]
fn a_bit_above_the_port_count_or_past_the_report_is_ignored() {
    // Un hub de cuatro puertos que marca el bit 6 miente o es basura.
    assert_eq!(hub_changed_ports(&[0b0100_0000], 4), alloc::vec![]);
    // Y un informe corto no se lee mas alla de lo que trajo.
    assert_eq!(hub_changed_ports(&[0b0000_0100], 15), alloc::vec![2]);
    assert_eq!(hub_changed_ports(&[], 7), alloc::vec![]);
}

#[test]
fn the_backstop_sweep_is_slower_than_the_sweep_that_is_the_only_signal() {
    // Si se igualaran, un hub con su endpoint armado pagaria una
    // transferencia de control por puerto y por segundo para nada.
    assert!(HUB_BACKSTOP_PERIOD_US > HUB_SCAN_PERIOD_US);
}

#[test]
fn every_change_bit_of_a_usb2_hub_has_the_feature_that_clears_it() {
    // Si la tabla se desalinea, un cambio se reconoce con la caracteristica
    // de otro y el bit se queda puesto: el hub vuelve a señalar ese puerto
    // en cada informe, para siempre.
    assert_eq!(
        HUB_PORT_CHANGES_USB2,
        [
            (1 << 0, 16), // C_PORT_CONNECTION
            (1 << 1, 17), // C_PORT_ENABLE
            (1 << 2, 18), // C_PORT_SUSPEND
            (1 << 3, 19), // C_PORT_OVER_CURRENT
            (1 << 4, 20), // C_PORT_RESET
            (1 << 5, 23), // C_PORT_L1, no C_BH_PORT_RESET
        ]
    );
}

#[test]
fn a_superspeed_hub_has_its_own_change_bits_and_they_are_not_the_usb2_ones() {
    assert_eq!(
        HUB_PORT_CHANGES_SS,
        [
            (1 << 0, 16), // C_PORT_CONNECTION
            (1 << 3, 19), // C_OVER_CURRENT
            (1 << 4, 20), // C_PORT_RESET
            (1 << 5, 29), // C_BH_PORT_RESET
            (1 << 6, 25), // C_PORT_LINK_STATE
            (1 << 7, 26), // C_PORT_CONFIG_ERROR
        ]
    );
    // Los bits 1 y 2 son reservados en SuperSpeed: mandarle un
    // CLEAR_FEATURE(C_PORT_ENABLE) a un hub SS es una peticion que no
    // existe en su protocolo.
    assert!(!HUB_PORT_CHANGES_SS
        .iter()
        .any(|&(bit, _)| bit == 1 << 1 || bit == 1 << 2));
}

#[test]
fn each_hub_is_given_the_table_of_its_own_protocol() {
    assert_eq!(hub_port_changes(SPEED_LOW), &HUB_PORT_CHANGES_USB2);
    assert_eq!(hub_port_changes(SPEED_FULL), &HUB_PORT_CHANGES_USB2);
    assert_eq!(hub_port_changes(SPEED_HIGH), &HUB_PORT_CHANGES_USB2);
    assert_eq!(hub_port_changes(SPEED_SUPER), &HUB_PORT_CHANGES_SS);
    // SuperSpeed Gen2 y lo que venga por encima siguen siendo SuperSpeed.
    assert_eq!(hub_port_changes(5), &HUB_PORT_CHANGES_SS);
    assert_eq!(hub_port_changes(6), &HUB_PORT_CHANGES_SS);
}

#[test]
fn no_change_bit_is_left_without_a_feature_to_clear_it() {
    // Un bit señalado que ninguna tabla sabe limpiar es el fallo entero:
    // el puerto se queda avisando y el driver releyendolo. Para cada
    // protocolo, todo bit que su wPortChange define tiene su fila.
    let usb2_defined: u16 = 0b0011_1111;
    let covered: u16 = HUB_PORT_CHANGES_USB2
        .iter()
        .fold(0, |acc, &(bit, _)| acc | bit);
    assert_eq!(covered, usb2_defined);
    let ss_defined: u16 = 0b1111_1001;
    let covered_ss: u16 = HUB_PORT_CHANGES_SS
        .iter()
        .fold(0, |acc, &(bit, _)| acc | bit);
    assert_eq!(covered_ss, ss_defined);
}
