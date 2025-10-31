//! # Architecture Module
//! 
//! Módulo de arquitectura específica

pub mod x64;

pub fn init() {
    x64::init();
}
