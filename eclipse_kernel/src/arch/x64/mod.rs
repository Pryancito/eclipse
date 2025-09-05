//! # x64 Architecture
//! 
//! Implementación específica para x64 con optimizaciones avanzadas

pub mod trap_frame;
pub mod optimizations;

use optimizations::X64Optimizations;

/// Inicializar arquitectura x64
pub fn init() {
    // Inicializar optimizaciones de CPU
    let _optimizations = X64Optimizations::new();
    
    // Configurar características de CPU
    configure_cpu_features();
}

/// Configurar características avanzadas de CPU
fn configure_cpu_features() {
    unsafe {
        // En un kernel bare metal, estas operaciones se harían con inline assembly
        // Por ahora, solo detectamos las características disponibles
        let _cpuid = core::arch::x86_64::__cpuid(1);
        let _cpuid_ext = core::arch::x86_64::__cpuid(7);
        
        // Las características se detectan pero no se muestran aquí
        // para evitar dependencias circulares con VGA
    }
}

/// Obtener información de la arquitectura
pub fn get_arch_info() -> &'static str {
    "x86_64 with advanced optimizations"
}
