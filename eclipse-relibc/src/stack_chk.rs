
#[cfg(all(not(any(test, feature = "host-testing")), any(eclipse_target, not(all(target_os = "linux", not(eclipse_target))))))]
#[no_mangle]
pub unsafe extern "C" fn __stack_chk_fail() -> ! {
    panic!("Stack check failed");
}
