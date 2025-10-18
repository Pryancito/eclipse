// Parche para mejorar el manejo de errores del driver AHCI
// Específico para Intel 200 Series PCH (device 0xa282)

use log::{debug, warn, error};

// Función para verificar si los errores son críticos o solo advertencias
fn is_critical_error(is: u32, ie: u32, cmd: u32, tfd: u32) -> bool {
    // Intel 200 Series PCH puede reportar errores menores que no son críticos
    // Verificamos si son errores reales o solo estados de transición
    
    // Error crítico: TFD bit 0 (error) o bit 5 (device fault)
    let tfd_error = (tfd & 0x21) != 0;
    
    // Error crítico: IS con bits de error reales (no solo completado)
    let is_critical = (is & 0xFFFF0000) != 0; // Solo bits altos son errores críticos
    
    // Error crítico: CMD con bits de error
    let cmd_error = (cmd & 0x40000000) != 0; // Bit 30 indica error
    
    tfd_error || is_critical || cmd_error
}

// Función mejorada para manejar errores del Intel 200 Series PCH
pub fn handle_ahci_error_intel_200_series(
    is: u32, ie: u32, cmd: u32, tfd: u32,
    ssts: u32, sctl: u32, serr: u32, sact: u32,
    ci: u32, sntf: u32, fbs: u32
) -> bool {
    if is_critical_error(is, ie, cmd, tfd) {
        // Error real - reportar como error
        error!("CRITICAL AHCI ERROR - Intel 200 Series PCH");
        error!("IS {:X} IE {:X} CMD {:X} TFD {:X}", is, ie, cmd, tfd);
        error!("SSTS {:X} SCTL {:X} SERR {:X} SACT {:X}", ssts, sctl, serr, sact);
        error!("CI {:X} SNTF {:X} FBS {:X}", ci, sntf, fbs);
        true
    } else {
        // Error menor - solo debug/warning
        debug!("Minor AHCI status - Intel 200 Series PCH (not critical)");
        debug!("IS {:X} IE {:X} CMD {:X} TFD {:X}", is, ie, cmd, tfd);
        debug!("SSTS {:X} SCTL {:X} SERR {:X} SACT {:X}", ssts, sctl, serr, sact);
        debug!("CI {:X} SNTF {:X} FBS {:X}", ci, sntf, fbs);
        false
    }
}

// Función para añadir delays específicos para Intel 200 Series PCH
pub fn intel_200_series_delay() {
    // Intel 200 Series PCH necesita más tiempo para estabilizarse
    std::thread::sleep(std::time::Duration::from_millis(10));
}
