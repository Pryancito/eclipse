//! Ejecuta tests de unidad, stress y benchmark del input_service.
//! Ejecutar: cargo run -p input_service --bin input_service_tests --features test
//! (desde eclipse_kernel/userspace/input_service o con --manifest-path)

fn main() {
    input_service::tests::run_all();
}
