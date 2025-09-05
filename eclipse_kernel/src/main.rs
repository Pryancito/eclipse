#![no_std]
#![no_main]

mod main_simple;

#[no_mangle]
pub extern "C" fn _start() -> ! {
    main_simple::kernel_main();
}



