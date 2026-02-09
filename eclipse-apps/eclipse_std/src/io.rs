//! I/O utilities for Eclipse OS applications

use eclipse_libc::write;
use core::fmt::{self, Write};

pub struct StdoutWriter;
impl Write for StdoutWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        write(1, s.as_bytes());
        Ok(())
    }
}

pub struct StderrWriter;
impl Write for StderrWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        write(2, s.as_bytes());
        Ok(())
    }
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {{
        use core::fmt::Write;
        let mut writer = $crate::io::StdoutWriter;
        let _ = write!(writer, $($arg)*);
    }};
}

#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => {{
        use core::fmt::Write;
        let mut writer = $crate::io::StdoutWriter;
        let _ = writeln!(writer, $($arg)*);
    }};
}

#[macro_export]
macro_rules! eprint {
    ($($arg:tt)*) => {{
        use core::fmt::Write;
        let mut writer = $crate::io::StderrWriter;
        let _ = write!(writer, $($arg)*);
    }};
}

#[macro_export]
macro_rules! eprintln {
    () => { $crate::eprint!("\n") };
    ($($arg:tt)*) => {{
        use core::fmt::Write;
        let mut writer = $crate::io::StderrWriter;
        let _ = writeln!(writer, $($arg)*);
    }};
}

pub use {print, println, eprint, eprintln};
