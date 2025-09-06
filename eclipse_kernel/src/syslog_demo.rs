//! Demostración del sistema de logging syslog
//! 
//! Muestra cómo usar el sistema de logging similar a syslog
//! en el kernel Eclipse OS.

use crate::syslog::{
    SyslogFacility, SyslogSeverity, 
    init_syslog, get_syslog_stats, set_syslog_level, enable_syslog,
    log_kernel, log_daemon, log_auth, log_mail
};
use alloc::format;

/// Demostrar el sistema de logging syslog
pub fn demonstrate_syslog() -> Result<(), &'static str> {
    // Inicializar sistema syslog
    init_syslog()?;
    
    // Configurar nivel de logging
    set_syslog_level(SyslogSeverity::Debug);
    
    // Habilitar logging
    enable_syslog(true);
    
    // Demostrar diferentes niveles de logging
    demonstrate_log_levels();
    
    // Demostrar diferentes facilidades
    demonstrate_facilities();
    
    // Demostrar estadísticas
    demonstrate_stats();
    
    Ok(())
}

fn demonstrate_log_levels() {
    // Emergency - sistema no usable
    log_kernel(SyslogSeverity::Emergency, "kernel", "Sistema en estado crítico");
    
    // Alert - acción requerida inmediatamente
    log_kernel(SyslogSeverity::Alert, "kernel", "Memoria crítica baja");
    
    // Critical - condiciones críticas
    log_kernel(SyslogSeverity::Critical, "kernel", "Error crítico en el kernel");
    
    // Error - condiciones de error
    log_kernel(SyslogSeverity::Error, "kernel", "Error en el sistema de archivos");
    
    // Warning - condiciones de advertencia
    log_kernel(SyslogSeverity::Warning, "kernel", "Temperatura del CPU alta");
    
    // Notice - condición normal pero significativa
    log_kernel(SyslogSeverity::Notice, "kernel", "Nuevo proceso iniciado");
    
    // Info - mensajes informativos
    log_kernel(SyslogSeverity::Info, "kernel", "Sistema iniciado correctamente");
    
    // Debug - mensajes de depuración
    log_kernel(SyslogSeverity::Debug, "kernel", "Variable x = 42");
}

fn demonstrate_facilities() {
    // Kernel messages
    log_kernel(SyslogSeverity::Info, "kernel", "Mensaje del kernel");
    
    // Daemon messages
    log_daemon(SyslogSeverity::Info, "systemd", "Servicio iniciado");
    
    // Authentication messages
    log_auth(SyslogSeverity::Warning, "auth", "Intento de login fallido");
    
    // Mail messages
    log_mail(SyslogSeverity::Error, "postfix", "Error enviando correo");
}

fn demonstrate_stats() {
    let stats = get_syslog_stats();
    
    // Log de las estadísticas
    log_kernel(SyslogSeverity::Info, "syslog", &format!(
        "Estadísticas: {} entradas, habilitado: {}, nivel: {}, puerto: {}",
        stats.total_entries,
        stats.enabled,
        stats.min_severity,
        stats.serial_port
    ));
}

/// Demostrar uso de macros syslog
pub fn demonstrate_syslog_macros() {
    use crate::{syslog_emerg, syslog_alert, syslog_crit, syslog_err, 
                syslog_warn, syslog_notice, syslog_info, syslog_debug};
    
    // Usar macros de conveniencia
    syslog_emerg!("kernel", "Sistema no usable");
    syslog_alert!("kernel", "Acción requerida inmediatamente");
    syslog_crit!("kernel", "Condición crítica");
    syslog_err!("kernel", "Error del sistema");
    syslog_warn!("kernel", "Advertencia del sistema");
    syslog_notice!("kernel", "Notificación importante");
    syslog_info!("kernel", "Información del sistema");
    syslog_debug!("kernel", "Información de depuración");
}

/// Demostrar logging con diferentes tags
pub fn demonstrate_logging_tags() {
    // Diferentes componentes del sistema
    log_kernel(SyslogSeverity::Info, "memory", "Gestor de memoria inicializado");
    log_kernel(SyslogSeverity::Info, "process", "Scheduler iniciado");
    log_kernel(SyslogSeverity::Info, "filesystem", "Sistema de archivos montado");
    log_kernel(SyslogSeverity::Info, "network", "Stack de red configurado");
    log_kernel(SyslogSeverity::Info, "security", "Sistema de seguridad activado");
    log_kernel(SyslogSeverity::Info, "ai", "Sistema de IA inicializado");
    log_kernel(SyslogSeverity::Info, "plugins", "Sistema de plugins cargado");
    log_kernel(SyslogSeverity::Info, "metrics", "Sistema de métricas activado");
    log_kernel(SyslogSeverity::Info, "config", "Sistema de configuración listo");
}

/// Demostrar logging de eventos del sistema
pub fn demonstrate_system_events() {
    // Eventos de inicialización
    log_kernel(SyslogSeverity::Info, "init", "Iniciando Eclipse OS");
    log_kernel(SyslogSeverity::Info, "init", "Cargando módulos del kernel");
    log_kernel(SyslogSeverity::Info, "init", "Inicializando hardware");
    log_kernel(SyslogSeverity::Info, "init", "Configurando memoria");
    log_kernel(SyslogSeverity::Info, "init", "Sistema listo");
    
    // Eventos de runtime
    log_kernel(SyslogSeverity::Notice, "runtime", "Nuevo proceso creado");
    log_kernel(SyslogSeverity::Notice, "runtime", "Proceso terminado");
    log_kernel(SyslogSeverity::Info, "runtime", "Cambio de contexto");
    log_kernel(SyslogSeverity::Info, "runtime", "Interrupción manejada");
    
    // Eventos de error
    log_kernel(SyslogSeverity::Error, "error", "Page fault en dirección 0x12345678");
    log_kernel(SyslogSeverity::Warning, "warning", "Timeout en operación I/O");
    log_kernel(SyslogSeverity::Critical, "critical", "Stack overflow detectado");
}

/// Demostrar logging de métricas del sistema
pub fn demonstrate_metrics_logging() {
    // Métricas de CPU
    log_kernel(SyslogSeverity::Info, "metrics", "CPU usage: 45%");
    log_kernel(SyslogSeverity::Info, "metrics", "Load average: 1.2, 1.5, 1.8");
    
    // Métricas de memoria
    log_kernel(SyslogSeverity::Info, "metrics", "Memory usage: 256MB/1GB");
    log_kernel(SyslogSeverity::Info, "metrics", "Free memory: 768MB");
    
    // Métricas de I/O
    log_kernel(SyslogSeverity::Info, "metrics", "Disk I/O: 120 ops/sec");
    log_kernel(SyslogSeverity::Info, "metrics", "Network I/O: 5MB/s");
    
    // Métricas de procesos
    log_kernel(SyslogSeverity::Info, "metrics", "Active processes: 15");
    log_kernel(SyslogSeverity::Info, "metrics", "Threads: 45");
}

/// Demostrar logging de seguridad
pub fn demonstrate_security_logging() {
    // Eventos de autenticación
    log_auth(SyslogSeverity::Info, "auth", "Usuario root autenticado");
    log_auth(SyslogSeverity::Warning, "auth", "Intento de login fallido para usuario 'hacker'");
    log_auth(SyslogSeverity::Critical, "auth", "Múltiples intentos de login fallidos");
    
    // Eventos de autorización
    log_auth(SyslogSeverity::Info, "auth", "Acceso concedido a /etc/passwd");
    log_auth(SyslogSeverity::Warning, "auth", "Acceso denegado a /root");
    
    // Eventos de auditoría
    log_auth(SyslogSeverity::Info, "audit", "Archivo modificado: /etc/hosts");
    log_auth(SyslogSeverity::Warning, "audit", "Cambio de permisos detectado");
}

/// Demostrar logging de red
pub fn demonstrate_network_logging() {
    // Eventos de conexión
    log_kernel(SyslogSeverity::Info, "network", "Conexión TCP establecida");
    log_kernel(SyslogSeverity::Info, "network", "Conexión UDP iniciada");
    
    // Eventos de error de red
    log_kernel(SyslogSeverity::Error, "network", "Timeout en conexión TCP");
    log_kernel(SyslogSeverity::Warning, "network", "Paquete corrupto recibido");
    
    // Eventos de seguridad de red
    log_kernel(SyslogSeverity::Warning, "network", "Intento de conexión sospechosa");
    log_kernel(SyslogSeverity::Critical, "network", "Ataque DDoS detectado");
}

/// Función principal de demostración
pub fn run_syslog_demo() -> Result<(), &'static str> {
    log_kernel(SyslogSeverity::Info, "demo", "Iniciando demostración de syslog");
    
    // Ejecutar todas las demostraciones
    demonstrate_log_levels();
    demonstrate_facilities();
    demonstrate_logging_tags();
    demonstrate_system_events();
    demonstrate_metrics_logging();
    demonstrate_security_logging();
    demonstrate_network_logging();
    demonstrate_syslog_macros();
    demonstrate_stats();
    
    log_kernel(SyslogSeverity::Info, "demo", "Demostración de syslog completada");
    
    Ok(())
}
