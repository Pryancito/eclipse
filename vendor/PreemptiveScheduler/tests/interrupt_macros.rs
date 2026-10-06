#[test]
fn exported_macros_resolve_from_a_downstream_crate() {
    let _on = || {
        executor::run_with_intr_saved_on! {
            core::hint::black_box(());
        }
    };
    let _off = || {
        executor::run_with_intr_saved_off! {
            core::hint::black_box(());
        }
    };
}
