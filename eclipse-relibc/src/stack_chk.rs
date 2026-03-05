
#[no_mangle]
pub unsafe extern "C" fn __stack_chk_fail() -> ! {
    panic!("Stack check failed");
}
