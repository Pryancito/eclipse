#![no_std]

pub const SPAWN_SERVICE: u32 = 0;

pub fn spawn_service_short_name(service_id: u32) -> &'static str {
    match service_id {
        0 => "spawn_service",
        _ => "unknown",
    }
}

pub mod spawn_service {
    pub const ID: u32 = 0;
}
