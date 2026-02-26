//! Tests de parsing IPC.
//!
//! Ejecutar con: `cargo test -p eclipse_ipc_tests --target x86_64-unknown-linux-gnu`

use core::mem::size_of;
use eclipse_ipc::{
    parse_fast, parse_slow,
    types::{
        build_input_pid_response_payload, build_subscribe_payload,
        EclipseMessage, GET_INPUT_PID_MSG, TAG_INPT, TAG_SUBS,
    },
};

#[test]
fn parse_fast_none_wrong_len() {
    let data = [0u8; 24];
    assert!(parse_fast(&data, 1, 0).is_none());
    assert!(parse_fast(&data, 1, 8).is_none());
    assert!(parse_fast(&data, 1, 23).is_none());
}

#[test]
fn parse_fast_some_input_event() {
    let data = [0u8; 24];
    let msg = parse_fast(&data, 42, 24);
    assert!(matches!(msg, Some(EclipseMessage::Input(_))));
    if let Some(EclipseMessage::Input(ev)) = msg {
        assert_eq!(core::mem::size_of_val(&ev), 24);
        assert_eq!(ev.device_id, 0);
        assert_eq!(ev.event_type, 0);
        assert_eq!(ev.value, 0);
    }
}

#[test]
fn parse_slow_subscribe() {
    let buf = build_subscribe_payload(0x1234_5678);
    let msg = parse_slow(&buf, 8, 99);
    match &msg {
        Some(EclipseMessage::Subscribe { subscriber_pid: x }) => assert_eq!(*x, 0x1234_5678),
        _ => panic!("expected Subscribe"),
    }
}

#[test]
fn parse_slow_get_input_pid() {
    let buf = *GET_INPUT_PID_MSG;
    let msg = parse_slow(&buf, 13, 1);
    assert!(matches!(msg, Some(EclipseMessage::GetInputPid)));
}

#[test]
fn parse_slow_input_pid_response() {
    let buf = build_input_pid_response_payload(7);
    let msg = parse_slow(&buf, 8, 1);
    match &msg {
        Some(EclipseMessage::InputPidResponse { pid: x }) => assert_eq!(*x, 7),
        _ => panic!("expected InputPidResponse"),
    }
}

#[test]
fn parse_slow_sidewind() {
    let msg = sidewind_core::SideWindMessage::new_commit();
    let bytes = unsafe {
        core::slice::from_raw_parts(
            &msg as *const _ as *const u8,
            size_of::<sidewind_core::SideWindMessage>(),
        )
    };
    let parsed = parse_slow(bytes, bytes.len(), 100);
    assert!(matches!(parsed, Some(EclipseMessage::SideWind(_, 100))));
}

#[test]
fn parse_slow_raw_unknown() {
    let buf = [0xABu8; 20];
    let msg = parse_slow(&buf, 20, 5);
    assert!(matches!(msg, Some(EclipseMessage::Raw { len: 20, from: 5, .. })));
    if let Some(EclipseMessage::Raw { data, len, from }) = msg {
        assert_eq!(len, 20);
        assert_eq!(from, 5);
        assert_eq!(&data[..20], &buf[..]);
    }
}

#[test]
fn parse_slow_empty_none() {
    let buf: [u8; 0] = [];
    assert!(parse_slow(&buf, 0, 0).is_none());
}

#[test]
fn build_subscribe_payload_roundtrip() {
    let pid = 0xDEAD_BEEFu32;
    let buf = build_subscribe_payload(pid);
    assert_eq!(buf[0..4], *TAG_SUBS);
    assert_eq!(u32::from_le_bytes(buf[4..8].try_into().unwrap()), pid);
    let msg = parse_slow(&buf, 8, 0);
    match &msg {
        Some(EclipseMessage::Subscribe { subscriber_pid: x }) => assert_eq!(*x, pid),
        _ => panic!("expected Subscribe with pid"),
    }
}

#[test]
fn build_input_pid_response_payload_roundtrip() {
    let pid = 42u32;
    let buf = build_input_pid_response_payload(pid);
    assert_eq!(buf[0..4], *TAG_INPT);
    assert_eq!(u32::from_le_bytes(buf[4..8].try_into().unwrap()), pid);
    let msg = parse_slow(&buf, 8, 1);
    match &msg {
        Some(EclipseMessage::InputPidResponse { pid: x }) => assert_eq!(*x, pid),
        _ => panic!("expected InputPidResponse with pid"),
    }
}
