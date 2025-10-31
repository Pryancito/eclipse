//! Módulo para gestionar los datos pasados por el bootloader.

// Estructura temporal para simular los datos del disco.
// En el futuro, esto será reemplazado por un puntero a memoria
// que el bootloader nos pasará.
static mut DISK_DATA: Option<&'static [u8]> = None;

pub unsafe fn set_disk_data(data: &'static [u8]) {
    DISK_DATA = Some(data);
}

pub unsafe fn get_disk_data() -> &'static [u8] {
    DISK_DATA.unwrap_or(&[])
}
