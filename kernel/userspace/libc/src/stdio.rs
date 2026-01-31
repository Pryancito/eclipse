//! Funciones de I/O estándar
use crate::syscall::write;

pub const STDOUT: u32 = 1;
pub const STDERR: u32 = 2;

pub fn puts(s: &str) {
    write(STDOUT, s.as_bytes());
}

pub fn putchar(c: char) {
    let mut buf = [0u8; 4];
    let s = c.encode_utf8(&mut buf);
    write(STDOUT, s.as_bytes());
}

pub struct StdoutWriter;

impl core::fmt::Write for StdoutWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        puts(s);
        Ok(())
    }
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {{
        use core::fmt::Write;
        let _ = write!($crate::stdio::StdoutWriter, $($arg)*);
    }};
}

#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => {{
        $crate::print!($($arg)*);
        $crate::print!("\n");
    }};
}
