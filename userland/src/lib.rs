//! # ReactOS Userland en Rust

pub mod shell;
pub mod services;
pub mod applications;

use anyhow::Result;

pub fn initialize() -> Result<()> {
    log::info!("Userland inicializado");
    Ok(())
}

pub fn execute_command(command: &str) -> Result<()> {
    shell::execute_command(command)
}

pub fn get_prompt() -> String {
    shell::get_prompt()
}
