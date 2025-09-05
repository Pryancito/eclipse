#![no_std]
#![no_main]

mod main_working;

#[no_mangle]
pub extern "C" fn _start() -> ! {
    main_working::_start();
}
