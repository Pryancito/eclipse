//! EclipseFS Server - Servidor de sistema de archivos para Eclipse OS
//! 
//! Este es el punto de entrada principal del servidor EclipseFS que se ejecuta
//! en espacio de usuario como parte de la arquitectura microkernel.

use anyhow::Result;
use eclipsefs::server::{EclipseFSServer, MicrokernelServer};

fn main() -> Result<()> {
    println!("\n╔═══════════════════════════════════════════════════════╗");
    println!("║        EclipseFS Server - Eclipse OS Microkernel     ║");
    println!("║                   Version {}                      ║", eclipsefs::ECLIPSEFS_SERVER_VERSION);
    println!("╚═══════════════════════════════════════════════════════╝\n");

    // Crear el servidor
    let mut server = EclipseFSServer::new();

    // Inicializar el servidor
    println!("Inicializando servidor...");
    server.initialize()?;

    println!("\n🚀 EclipseFS Server está ejecutándose");
    println!("   - Nombre: {}", server.name());
    println!("   - Tipo de mensaje: {:?}", server.message_type());
    println!("   - Prioridad: {}", server.priority());
    
    println!("\n📝 Servidor listo para recibir mensajes del microkernel");
    println!("   Presione Ctrl+C para detener el servidor\n");

    // En un servidor real, aquí entraríamos en un loop de procesamiento de mensajes
    // Por ahora, simplemente mostramos un mensaje de ejemplo y limpiamos
    
    println!("   [Modo demostración - el servidor se detendrá ahora]");
    
    // Simular procesamiento de algunos mensajes de ejemplo
    println!("\n📨 Procesando mensajes de ejemplo...\n");
    
    // Ejemplo 1: Montar filesystem
    let mount_msg = create_example_message(15, b"/dev/sda1");
    if let Ok(_response) = server.process_message(&mount_msg) {
        println!("   ✓ Mensaje MOUNT procesado correctamente");
    }
    
    // Ejemplo 2: Listar directorio raíz
    let list_msg = create_example_message(7, b"/");
    if let Ok(response) = server.process_message(&list_msg) {
        println!("   ✓ Mensaje LIST procesado correctamente");
        println!("     Respuesta: {}", String::from_utf8_lossy(&response));
    }
    
    // Ejemplo 3: Crear archivo
    let create_msg = create_example_message(5, b"\x00\x00\x00\x00/test.txt");
    if let Ok(_response) = server.process_message(&create_msg) {
        println!("   ✓ Mensaje CREATE procesado correctamente");
    }

    // Obtener estadísticas
    println!("\n📊 Estadísticas del servidor:");
    let stats = server.get_stats();
    println!("   - Mensajes procesados: {}", stats.messages_processed);
    println!("   - Mensajes fallidos: {}", stats.messages_failed);
    if let Some(ref error) = stats.last_error {
        println!("   - Último error: {}", error);
    }

    // Detener el servidor
    println!("\nDeteniendo servidor...");
    server.shutdown()?;

    println!("╔═══════════════════════════════════════════════════════╗");
    println!("║          EclipseFS Server detenido exitosamente      ║");
    println!("╚═══════════════════════════════════════════════════════╝\n");

    Ok(())
}

/// Crear un mensaje de ejemplo para demostración
fn create_example_message(command: u8, data: &[u8]) -> eclipsefs::Message {
    use eclipsefs::MessageType;
    
    let mut message = eclipsefs::Message {
        id: 0,
        from: 0,
        to: 0,
        message_type: MessageType::FileSystem,
        data: [0u8; 256],
        data_size: 0,
        priority: 10,
        flags: 0,
        reserved: [0; 2],
    };

    // Primer byte es el comando
    message.data[0] = command;
    
    // Copiar datos del comando
    let data_len = std::cmp::min(data.len(), 255);
    message.data[1..1+data_len].copy_from_slice(&data[..data_len]);
    message.data_size = (1 + data_len) as u32;

    message
}
