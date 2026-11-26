//! The driver's `log` records, on stderr.
//!
//! In the kernel they reach the kernel's log; in this process nothing took
//! them, so every line of the driver and of the RM that goes through `log`
//! (the console GPU's bring-up among them) was lost. eclipse-init keeps the
//! service's stderr in `/tmp/unvidia.log`.

use log::{LevelFilter, Log, Metadata, Record};

struct StderrLog;

impl Log for StderrLog {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            eprintln!("{}", line(record));
        }
    }

    fn flush(&self) {}
}

/// One record as its line: who wrote it, how loud, what.
fn line(record: &Record) -> String {
    format!(
        "{} [{}] {}: {}",
        crate::NAME,
        record.level(),
        record.target(),
        record.args()
    )
}

static LOGGER: StderrLog = StderrLog;

/// Take the driver's records from here on, `Info` and louder (the RM turns
/// the level down and back up itself around its noisy stretches).
pub fn install() {
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(LevelFilter::Info);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_is_one_line_with_its_level_and_target() {
        let args = format_args!("step9 load_status={}", 0x40);
        let record = Record::builder()
            .args(args)
            .level(log::Level::Warn)
            .target("zcore_drivers::display::nvidia")
            .build();
        assert_eq!(
            line(&record),
            "udrv-nvidia [WARN] zcore_drivers::display::nvidia: step9 load_status=64"
        );
    }
}
