//! Ejecuta los tests unitarios de eclipse_ipc en el host.
//! Compilar y ejecutar:
//!   cargo run -p eclipse_ipc --example test_runner --features testable,host-testing

fn main() {
    println!("[IPC-TEST] Running eclipse_ipc unit tests...");

    eclipse_ipc::tests::tests::test_build_subscribe_payload();
    eclipse_ipc::tests::tests::test_build_input_pid_response_payload();
    eclipse_ipc::tests::tests::test_encode_fast_input_event();

    eclipse_ipc::tests::tests::test_parse_fast_input_event();
    eclipse_ipc::tests::tests::test_parse_fast_input_pid_response();
    eclipse_ipc::tests::tests::test_parse_fast_network_pid_response();
    eclipse_ipc::tests::tests::test_parse_fast_net_stats_response();
    eclipse_ipc::tests::tests::test_parse_fast_invalid_len();

    eclipse_ipc::tests::tests::test_parse_slow_sidewind();
    eclipse_ipc::tests::tests::test_parse_slow_subscribe();
    eclipse_ipc::tests::tests::test_parse_slow_control_requests();
    eclipse_ipc::tests::tests::test_parse_slow_raw_fallback();

    eclipse_ipc::tests::tests::test_parse_fast_zero_len();
    eclipse_ipc::tests::tests::test_parse_fast_unknown_tag_returns_none();
    eclipse_ipc::tests::tests::test_parse_fast_net_stats_boundary();
    eclipse_ipc::tests::tests::test_parse_slow_zero_len();
    eclipse_ipc::tests::tests::test_build_subscribe_roundtrip();
    eclipse_ipc::tests::tests::test_build_input_pid_response_roundtrip();
    eclipse_ipc::tests::tests::test_encode_fast_input_event_roundtrip();
    eclipse_ipc::tests::tests::test_parse_slow_service_info_response();
    eclipse_ipc::tests::tests::test_parse_slow_input_via_slow_path();

    eclipse_ipc::tests::tests::test_constants_max_msg_len_and_tags();
    eclipse_ipc::tests::tests::test_ipc_channel_new();
    eclipse_ipc::tests::tests::test_services_constants();
    eclipse_ipc::tests::tests::test_eclipse_message_input_clone_debug();

    println!("[IPC-TEST] All eclipse_ipc tests passed successfully!");
}
