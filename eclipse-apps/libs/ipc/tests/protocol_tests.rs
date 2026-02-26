use eclipse_ipc::prelude::*;
use eclipse_ipc::types::{parse_fast, parse_slow};

#[cfg(test)]
#[allow(unused_imports)]
mod test_suite {
    use super::*;
    use core::assert_eq;
    use core::panic;
    #[test]
fn test_input_event_fast_path() {
    let ev = eclipse_libc::InputEvent {
        device_id: 1,
        event_type: 0,
        code: 30, // 'A' key
        value: 1,  // Pressed
        timestamp: 12345,
    };

    let encoded = ev.encode_fast();
    let decoded = parse_fast(&encoded, 10, 24).expect("Failed to parse fast path message");

    if let EclipseMessage::Input(dev_ev) = decoded {
        assert_eq!(dev_ev.device_id, 1);
        assert_eq!(dev_ev.code, 30);
        assert_eq!(dev_ev.value, 1);
    } else {
        panic!("Expected EclipseMessage::Input");
    }
}

#[test]
fn test_subscribe_slow_path() {
    let self_pid = 42;
    let payload = eclipse_ipc::types::build_subscribe_payload(self_pid);
    
    let decoded = parse_slow(&payload, payload.len(), 1).expect("Failed to parse subscribe message");
    
    if let EclipseMessage::Subscribe { subscriber_pid } = decoded {
        assert_eq!(subscriber_pid, 42);
    } else {
        panic!("Expected EclipseMessage::Subscribe");
    }
}

#[test]
fn test_input_pid_response_slow_path() {
    let input_pid = 100;
    let payload = eclipse_ipc::types::build_input_pid_response_payload(input_pid);
    
    let decoded = parse_slow(&payload, payload.len(), 1).expect("Failed to parse input pid response");
    
    if let EclipseMessage::InputPidResponse { pid } = decoded {
        assert_eq!(pid, 100);
    } else {
        panic!("Expected EclipseMessage::InputPidResponse");
    }
}

#[test]
fn test_raw_fallback() {
    let data = b"some random data";
    let decoded = parse_slow(data, data.len(), 50).expect("Failed to parse raw message");
    
    if let EclipseMessage::Raw { data: raw_data, len, from } = decoded {
        assert_eq!(from, 50);
        assert_eq!(len, data.len());
        assert_eq!(&raw_data[..len], data);
    } else {
        panic!("Expected EclipseMessage::Raw");
    }
}
}
