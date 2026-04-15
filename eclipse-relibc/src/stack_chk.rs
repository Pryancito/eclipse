
#[cfg(all(not(any(test, feature = "host-testing")), eclipse_target))]
#[no_mangle]
pub unsafe extern "C" fn __stack_chk_fail() -> ! {
    panic!("Stack check failed");
}
