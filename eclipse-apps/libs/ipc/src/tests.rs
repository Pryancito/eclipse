#[cfg(any(test, feature = "testable"))]
pub mod tests {
    use crate::types::{
        EclipseMessage, parse_fast, parse_slow, build_subscribe_payload,
        build_input_pid_response_payload, MAX_MSG_LEN, TAG_INPT, TAG_NETW, TAG_NSTA, TAG_SUBS,
        TAG_SVCS, GET_INPUT_PID_MSG, GET_NETWORK_PID_MSG, GET_NET_STATS_MSG, TAG_SWND, TAG_WAYL,
    };
    use crate::protocol::EclipseEncode;
    use crate::channel::IpcChannel;
    use crate::services::{INIT_PID, MSG_TYPE_INPUT, DEFAULT_INPUT_QUERY_ATTEMPTS};
    use eclipse_syscall::InputEvent;
    use sidewind_core::{SideWindMessage, SIDEWIND_TAG};

    // =========================================================================
    // Protocol Builder Tests
    // =========================================================================

    #[test]
    pub fn test_build_subscribe_payload() {
        let payload = build_subscribe_payload(0x12345678);
        assert_eq!(&payload[0..4], TAG_SUBS);
        assert_eq!(&payload[4..8], &[0x78, 0x56, 0x34, 0x12]);
    }

    #[test]
    pub fn test_build_input_pid_response_payload() {
        let payload = build_input_pid_response_payload(0x87654321);
        assert_eq!(&payload[0..4], TAG_INPT);
        assert_eq!(&payload[4..8], &[0x21, 0x43, 0x65, 0x87]);
    }

    #[test]
    pub fn test_encode_fast_input_event() {
        let ev = InputEvent {
            device_id: 1,
            event_type: 2,
            code: 3,
            value: 4,
            timestamp: 5,
        };
        let encoded = ev.encode_fast();
        assert_eq!(encoded.len(), 24);
        
        let decoded = unsafe { core::ptr::read_unaligned(encoded.as_ptr() as *const InputEvent) };
        assert_eq!(decoded.device_id, 1);
        assert_eq!(decoded.event_type, 2);
        assert_eq!(decoded.code, 3);
        assert_eq!(decoded.value, 4);
        assert_eq!(decoded.timestamp, 5);
        
        assert_eq!(InputEvent::msg_type(), crate::services::MSG_TYPE_INPUT);
        assert_eq!(InputEvent::data_size(), 24);
    }

    // =========================================================================
    // Fast Path Parser Tests
    // =========================================================================

    #[test]
    pub fn test_parse_fast_input_event() {
        let ev = InputEvent {
            device_id: 10,
            event_type: 11,
            code: 12,
            value: 13,
            timestamp: 14,
        };
        let data = ev.encode_fast();
        let parsed = parse_fast(&data, 0, 24);
        
        match parsed {
            Some(EclipseMessage::Input(parsed_ev)) => {
                assert_eq!(parsed_ev.device_id, 10);
            }
            _ => panic!("Failed to parse fast InputEvent"),
        }
    }

    #[test]
    pub fn test_parse_fast_input_pid_response() {
        let mut data = [0u8; 24];
        let payload = build_input_pid_response_payload(42);
        data[..8].copy_from_slice(&payload);
        
        let parsed = parse_fast(&data, 0, 8);
        match parsed {
            Some(EclipseMessage::InputPidResponse { pid }) => assert_eq!(pid, 42),
            _ => panic!("Failed to parse fast InputPidResponse"),
        }
    }

    #[test]
    pub fn test_parse_fast_network_pid_response() {
        let mut data = [0u8; 24];
        data[0..4].copy_from_slice(TAG_NETW);
        data[4..8].copy_from_slice(&99u32.to_le_bytes());
        
        let parsed = parse_fast(&data, 0, 8);
        match parsed {
            Some(EclipseMessage::NetworkPidResponse { pid }) => assert_eq!(pid, 99),
            _ => panic!("Failed to parse fast NetworkPidResponse"),
        }
    }

    #[test]
    pub fn test_parse_fast_net_stats_response() {
        let mut data = [0u8; 24];
        data[0..4].copy_from_slice(TAG_NSTA);
        data[4..12].copy_from_slice(&1024u64.to_le_bytes());
        data[12..20].copy_from_slice(&2048u64.to_le_bytes());
        
        let parsed = parse_fast(&data, 0, 20);
        match parsed {
            Some(EclipseMessage::NetStatsResponse { rx, tx }) => {
                assert_eq!(rx, 1024);
                assert_eq!(tx, 2048);
            }
            _ => panic!("Failed to parse fast NetStatsResponse"),
        }
    }

    #[test]
    pub fn test_parse_fast_invalid_len() {
        let mut data = [0u8; 24];
        data[0..4].copy_from_slice(TAG_INPT);
        data[4..8].copy_from_slice(&42u32.to_le_bytes());
        
        // length is wrong for an InputPidResponse (should be >= 8)
        let parsed = parse_fast(&data, 0, 7);
        assert!(parsed.is_none());
    }

    #[test]
    pub fn test_parse_fast_wayland() {
        let mut data = [0u8; 24];
        data[0..4].copy_from_slice(TAG_WAYL);
        data[4..8].copy_from_slice(&1u32.to_le_bytes()); // obj_id
        data[8..12].copy_from_slice(&((12u32 << 16) | 1u32).to_le_bytes()); // size_op
        data[12..16].copy_from_slice(&2u32.to_le_bytes()); // registry_id arg
        
        let parsed = parse_fast(&data, 42, 16);
        match parsed {
            Some(EclipseMessage::Wayland { data: _, len, from }) => {
                assert_eq!(len, 12);
                assert_eq!(from, 42);
            }
            _ => panic!("Failed to parse fast Wayland message"),
        }
    }

    // =========================================================================
    // Slow Path Parser Tests
    // =========================================================================

    #[test]
    pub fn test_parse_slow_sidewind() {
        let mut sw_msg = SideWindMessage {
            tag: core::u32::MAX, // default
            op: 1,
            x: 2,
            y: 3,
            w: 4,
            h: 5,
            name: [0u8; 32],
        };
        sw_msg.tag = SIDEWIND_TAG;
        
        let len = core::mem::size_of::<SideWindMessage>();
        let mut buf = [0u8; 128];
        unsafe {
            core::ptr::copy_nonoverlapping(
                &sw_msg as *const SideWindMessage as *const u8,
                buf.as_mut_ptr(),
                len,
            );
        }
        
        // OVERRIDE tag area intentionally to mock it fully, then restore
        // wait, we already set the tag directly inside the struct.
        buf[0..4].copy_from_slice(TAG_SWND);
        
        let parsed = parse_slow(&buf, len, 999);
        match parsed {
            Some(EclipseMessage::SideWind(parsed_sw, from)) => {
                assert_eq!(from, 999);
                assert_eq!(parsed_sw.op, 1);
                assert_eq!(parsed_sw.x, 2);
            }
            _ => panic!("Failed to parse slow SideWindMessage"),
        }
    }

    #[test]
    pub fn test_parse_slow_subscribe() {
        let payload = build_subscribe_payload(1234);
        let parsed = parse_slow(&payload, 8, 0);
        match parsed {
            Some(EclipseMessage::Subscribe { subscriber_pid: pid }) => assert_eq!(pid, 1234),
            _ => panic!("Failed to parse slow Subscribe"),
        }
    }

    #[test]
    pub fn test_parse_slow_control_requests() {
        // GET_INPUT_PID
        assert!(matches!(
            parse_slow(GET_INPUT_PID_MSG, GET_INPUT_PID_MSG.len(), 0),
            Some(EclipseMessage::GetInputPid)
        ));
        
        // GET_NETWORK_PID
        assert!(matches!(
            parse_slow(GET_NETWORK_PID_MSG, GET_NETWORK_PID_MSG.len(), 0),
            Some(EclipseMessage::GetNetworkPid)
        ));
        
        // GET_NET_STATS
        assert!(matches!(
            parse_slow(GET_NET_STATS_MSG, GET_NET_STATS_MSG.len(), 0),
            Some(EclipseMessage::GetNetStats)
        ));
    }

    #[test]
    pub fn test_parse_slow_wayland() {
        let mut buf = [0u8; 256];
        buf[0..4].copy_from_slice(TAG_WAYL);
        // 100 bytes of mock data
        buf[4..104].copy_from_slice(&[0xAA; 100]);
        
        let parsed = parse_slow(&buf, 104, 123);
        match parsed {
            Some(EclipseMessage::Wayland { data, len, from }) => {
                assert_eq!(len, 100);
                assert_eq!(from, 123);
                assert_eq!(data[0], 0xAA);
            }
            _ => panic!("Failed to parse slow Wayland message"),
        }
    }

    #[test]
    pub fn test_parse_slow_raw_fallback() {
        let mut buf = [0u8; 520]; // Slightly larger than MAX_MSG_LEN (512)
        buf[0..4].copy_from_slice(b"WXYZ"); // Unknown tag
        
        // len is clamped to MAX_MSG_LEN returning length
        let parsed = parse_slow(&buf, 520, 777);
        match parsed {
            Some(EclipseMessage::Raw { data, len, from }) => {
                assert_eq!(len, 512); // it clamps to MAX_MSG_LEN internally
                assert_eq!(from, 777);
                assert_eq!(&data[0..4], b"WXYZ");
            }
            _ => panic!("Failed to parse Unknown as Raw"),
        }
    }

    // =========================================================================
    // Bordes y roundtrip
    // =========================================================================

    #[test]
    pub fn test_parse_fast_zero_len() {
        let data = [0u8; 24];
        assert!(parse_fast(&data, 0, 0).is_none());
    }

    #[test]
    pub fn test_parse_fast_unknown_tag_returns_none() {
        let mut data = [0u8; 24];
        data[0..4].copy_from_slice(b"????");
        data[4..8].copy_from_slice(&1u32.to_le_bytes());
        assert!(parse_fast(&data, 0, 8).is_none());
    }

    #[test]
    pub fn test_parse_fast_net_stats_boundary() {
        let mut data = [0u8; 24];
        data[0..4].copy_from_slice(TAG_NSTA);
        data[4..12].copy_from_slice(&100u64.to_le_bytes());
        data[12..20].copy_from_slice(&200u64.to_le_bytes());
        assert!(parse_fast(&data, 0, 19).is_none());
        let parsed = parse_fast(&data, 0, 20);
        match parsed {
            Some(EclipseMessage::NetStatsResponse { rx, tx }) => {
                assert_eq!(rx, 100);
                assert_eq!(tx, 200);
            }
            _ => panic!("Expected NetStatsResponse at len 20"),
        }
    }

    #[test]
    pub fn test_parse_slow_zero_len() {
        let buf = [0u8; 8];
        assert!(parse_slow(&buf, 0, 0).is_none());
    }

    #[test]
    pub fn test_build_subscribe_roundtrip() {
        let pid = 0xDEAD_BEEF;
        let payload = build_subscribe_payload(pid);
        let parsed = parse_slow(&payload, 8, 0);
        match parsed {
            Some(EclipseMessage::Subscribe { subscriber_pid }) => assert_eq!(subscriber_pid, pid),
            _ => panic!("Subscribe roundtrip failed"),
        }
    }

    #[test]
    pub fn test_build_input_pid_response_roundtrip() {
        let pid = 12345;
        let payload = build_input_pid_response_payload(pid);
        let mut data = [0u8; 24];
        data[..8].copy_from_slice(&payload);
        let parsed = parse_fast(&data, 0, 8);
        match parsed {
            Some(EclipseMessage::InputPidResponse { pid: p }) => assert_eq!(p, pid),
            _ => panic!("InputPidResponse roundtrip failed"),
        }
    }

    #[test]
    pub fn test_encode_fast_input_event_roundtrip() {
        let ev = InputEvent {
            device_id: 100,
            event_type: 1,
            code: 2,
            value: 3,
            timestamp: 999,
        };
        let encoded = ev.encode_fast();
        let parsed = parse_fast(&encoded, 0, 24);
        match parsed {
            Some(EclipseMessage::Input(parsed_ev)) => {
                assert_eq!(parsed_ev.device_id, ev.device_id);
                assert_eq!(parsed_ev.event_type, ev.event_type);
                assert_eq!(parsed_ev.code, ev.code);
                assert_eq!(parsed_ev.value, ev.value);
                assert_eq!(parsed_ev.timestamp, ev.timestamp);
            }
            _ => panic!("InputEvent roundtrip failed"),
        }
    }

    // =========================================================================
    // Slow path: ServiceInfoResponse e Input por slow path
    // =========================================================================

    #[test]
    pub fn test_parse_slow_service_info_response() {
        let mut buf = [0u8; 64];
        buf[0..4].copy_from_slice(TAG_SVCS);
        buf[4..12].copy_from_slice(b"services"); // 8 bytes
        let parsed = parse_slow(&buf, 12, 1);
        match parsed {
            Some(EclipseMessage::ServiceInfoResponse { data, len }) => {
                assert_eq!(len, 12);
                assert_eq!(&data[0..4], TAG_SVCS);
                assert_eq!(&data[4..12], b"services");
            }
            _ => panic!("Failed to parse ServiceInfoResponse"),
        }
    }

    #[test]
    pub fn test_parse_slow_input_via_slow_path() {
        let ev = InputEvent {
            device_id: 7,
            event_type: 0,
            code: 1,
            value: 2,
            timestamp: 3,
        };
        let mut buf = [0u8; 32];
        unsafe {
            core::ptr::copy_nonoverlapping(
                &ev as *const InputEvent as *const u8,
                buf.as_mut_ptr(),
                core::mem::size_of::<InputEvent>(),
            );
        }
        let parsed = parse_slow(&buf, core::mem::size_of::<InputEvent>(), 42);
        match parsed {
            Some(EclipseMessage::Input(parsed_ev)) => {
                assert_eq!(parsed_ev.device_id, 7);
                assert_eq!(parsed_ev.timestamp, 3);
            }
            _ => panic!("Failed to parse Input via slow path"),
        }
    }

    // =========================================================================
    // Constantes y tipos
    // =========================================================================

    #[test]
    pub fn test_constants_max_msg_len_and_tags() {
        assert_eq!(MAX_MSG_LEN, 512);
        assert_eq!(TAG_SUBS, b"SUBS");
        assert_eq!(TAG_INPT, b"INPT");
        assert_eq!(TAG_NETW, b"NETW");
        assert_eq!(TAG_NSTA, b"NSTA");
        assert_eq!(TAG_SWND, b"SWND");
        assert_eq!(TAG_SVCS, b"SVCS");
        assert_eq!(GET_INPUT_PID_MSG, b"GET_INPUT_PID");
        assert_eq!(GET_NETWORK_PID_MSG, b"GET_NETWORK_PID");
        assert_eq!(GET_NET_STATS_MSG, b"GET_NET_STATS");
    }

    #[test]
    pub fn test_ipc_channel_new() {
        let ch = IpcChannel::new();
        assert_eq!(ch.message_count, 0);
    }

    #[test]
    pub fn test_services_constants() {
        assert_eq!(INIT_PID, 1);
        assert_eq!(MSG_TYPE_INPUT, 0x00000040);
        assert!(DEFAULT_INPUT_QUERY_ATTEMPTS > 0);
    }

    #[test]
    pub fn test_eclipse_message_input_clone_debug() {
        let ev = InputEvent {
            device_id: 1,
            event_type: 0,
            code: 0,
            value: 0,
            timestamp: 0,
        };
        let msg = EclipseMessage::Input(ev);
        let cloned = msg.clone();
        match (&msg, &cloned) {
            (EclipseMessage::Input(a), EclipseMessage::Input(b)) => {
                assert_eq!(a.device_id, b.device_id);
            }
            _ => panic!("expected Input variant"),
        }
    }

    // =========================================================================
    // Stress tests (muchas iteraciones para detectar fugas o inestabilidad)
    // =========================================================================

    #[test]
    pub fn test_stress_parse_fast_roundtrip() {
        const ITERS: u32 = 50_000;
        for i in 0..ITERS {
            let ev = InputEvent {
                device_id: (i % 256) as u32,
                event_type: (i % 4) as u8,
                code: (i % 256) as u16,
                value: i as i32,
                timestamp: i as u64,
            };
            let encoded = ev.encode_fast();
            let parsed = parse_fast(&encoded, 0, 24);
            match parsed {
                Some(EclipseMessage::Input(p)) => {
                    assert_eq!(p.device_id, ev.device_id);
                    assert_eq!(p.timestamp, ev.timestamp);
                }
                _ => panic!("stress roundtrip failed at iter {}", i),
            }
        }
    }

    #[test]
    pub fn test_stress_build_and_parse_slow() {
        const ITERS: u32 = 50_000;
        for i in 0..ITERS {
            let pid = (i % 0xFFFF_FFFF) as u32;
            let payload = build_subscribe_payload(pid);
            let parsed = parse_slow(&payload, 8, 0);
            match parsed {
                Some(EclipseMessage::Subscribe { subscriber_pid }) => assert_eq!(subscriber_pid, pid),
                _ => panic!("stress subscribe roundtrip failed at iter {}", i),
            }
        }
    }

    #[test]
    pub fn test_stress_parse_fast_net_stats() {
        let mut data = [0u8; 24];
        data[0..4].copy_from_slice(TAG_NSTA);
        const ITERS: u32 = 100_000;
        for i in 0..ITERS {
            let rx = (i as u64).wrapping_mul(7);
            let tx = (i as u64).wrapping_mul(11);
            data[4..12].copy_from_slice(&rx.to_le_bytes());
            data[12..20].copy_from_slice(&tx.to_le_bytes());
            let parsed = parse_fast(&data, 0, 20);
            match parsed {
                Some(EclipseMessage::NetStatsResponse { rx: r, tx: t }) => {
                    assert_eq!(r, rx);
                    assert_eq!(t, tx);
                }
                _ => panic!("stress net_stats failed at iter {}", i),
            }
        }
    }
}
