// The test harness supplies its own `main`, so this entry point is only the
// program's when we are not building the test binary.
#[cfg(not(test))]
#[unsafe(no_mangle)]
fn main() {
    crate::primary_main(kernel_hal::KernelConfig);
}
