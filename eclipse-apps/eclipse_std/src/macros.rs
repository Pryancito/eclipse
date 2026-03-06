//! Macros for Eclipse STD

/// Print macro for stdout
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {
        {
            // Eclipse target: no escribir a FD=1 (serial).
            // En host-testing mantenemos output para depuración.
            #[cfg(feature = "host-testing")]
            {
                use $crate::alloc::format;
                let s = format!($($arg)*);
                let _ = eclipse_syscall::call::write(1, s.as_bytes());
            }
        }
    };
}

/// Println macro for stdout with newline
#[macro_export]
macro_rules! println {
    () => {
        {
            #[cfg(feature = "host-testing")]
            {
                let _ = eclipse_syscall::call::write(1, b"\n");
            }
        }
    };
    ($($arg:tt)*) => {
        {
            #[cfg(feature = "host-testing")]
            {
                use $crate::alloc::format;
                let mut s = format!($($arg)*);
                s.push('\n');
                let _ = eclipse_syscall::call::write(1, s.as_bytes());
            }
        }
    };
}

/// Eprint macro for stderr
#[macro_export]
macro_rules! eprint {
    ($($arg:tt)*) => {
        {
            use $crate::alloc::format;
            let s = format!($($arg)*);
            let _ = eclipse_syscall::call::write(2, s.as_bytes());
        }
    };
}

/// Eprintln macro for stderr with newline
#[macro_export]
macro_rules! eprintln {
    () => {
        {
            let _ = eclipse_syscall::call::write(2, b"\n");
        }
    };
    ($($arg:tt)*) => {
        {
            use $crate::alloc::format;
            let mut s = format!($($arg)*);
            s.push('\n');
            let _ = eclipse_syscall::call::write(2, s.as_bytes());
        }
    };
}
