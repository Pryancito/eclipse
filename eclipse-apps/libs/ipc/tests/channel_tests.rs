use eclipse_ipc::prelude::*;
use eclipse_libc::{mock_clear, mock_push_receive, mock_push_receive_fast, mock_get_sent};

#[test]
fn test_channel_recv_fast() {
    mock_clear();
    
    // Simulate a fast path message (InputEvent)
    let ev = eclipse_libc::InputEvent {
        device_id: 1,
        event_type: 0,
        code: 30, // 'A'
        value: 1,
        timestamp: 12345,
    };
    
    let encoded = ev.encode_fast();
    mock_push_receive_fast(encoded, 100, 24);
    
    let mut ch = IpcChannel::new();
    let msg = ch.recv().expect("Should have received a message");
    
    if let EclipseMessage::Input(dev_ev) = msg {
        assert_eq!(dev_ev.device_id, 1);
        assert_eq!(dev_ev.code, 30);
    } else {
        panic!("Expected EclipseMessage::Input");
    }
    
    assert_eq!(ch.message_count, 1);
}

#[test]
fn test_channel_recv_slow() {
    mock_clear();
    
    // Simulate a slow path message (Subscribe)
    let payload = eclipse_ipc::types::build_subscribe_payload(42);
    mock_push_receive(payload.to_vec(), 200);
    
    let mut ch = IpcChannel::new();
    let msg = ch.recv().expect("Should have received a message");
    
    if let EclipseMessage::Subscribe { subscriber_pid } = msg {
        assert_eq!(subscriber_pid, 42);
    } else {
        panic!("Expected EclipseMessage::Subscribe");
    }
}

#[test]
fn test_channel_send() {
    mock_clear();
    
    IpcChannel::send_subscribe(500, 600);
    
    let sent = mock_get_sent();
    assert_eq!(sent.len(), 1);
    let (dest, msg_type, data) = &sent[0];
    assert_eq!(*dest, 500);
    assert_eq!(*msg_type, eclipse_ipc::prelude::MSG_TYPE_INPUT);
    
    // Check payload (Subscribe tag + PID)
    assert!(data.starts_with(b"SUB"));
}

#[test]
fn test_channel_recv_empty() {
    mock_clear();
    let mut ch = IpcChannel::new();
    assert!(ch.recv().is_none());
}
