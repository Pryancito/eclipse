//! `eclipse-labwc` — entry point.

fn main() -> anyhow::Result<()> {
    eprintln!("[eclipse-labwc] {}", eclipse_labwc::LABWC_COMPAT);
    eclipse_labwc::server::run()
}
