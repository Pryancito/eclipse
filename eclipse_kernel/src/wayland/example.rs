//! Ejemplo de uso del sistema Wayland para Eclipse OS
//! 
//! Demuestra cómo usar el servidor, cliente y aplicaciones Wayland.

use super::server::*;
use super::client_api::*;
use super::apps::*;
use super::rendering::*;
use super::surface::BufferFormat;
use alloc::string::{String, ToString};

/// Ejemplo de servidor Wayland
pub fn run_wayland_server_example() -> Result<(), &'static str> {
    // Crear servidor
    let mut server = WaylandServer::new(0); // Puerto 0 = socket Unix
    
    // Inicializar servidor
    server.initialize()?;
    
    // Ejecutar servidor
    server.run()?;
    
    // Obtener estadísticas
    let _stats = server.get_stats();
    
    // Detener servidor
    server.stop();
    
    Ok(())
}

/// Ejemplo de cliente Wayland
pub fn run_wayland_client_example() -> Result<(), &'static str> {
    // Crear cliente
    let mut client = WaylandClientAPI::new("/tmp/wayland-0".to_string());
    
    // Conectar al servidor
    client.connect()?;
    
    // Crear superficie
    let _surface_id = client.create_surface()?;
    
    // Crear shell surface
    let shell_surface_id = client.create_shell_surface(_surface_id)?;
    
    // Configurar ventana
    client.set_window_title(shell_surface_id, "Eclipse OS - Cliente Wayland")?;
    client.set_app_id(shell_surface_id, "eclipse-wayland-client")?;
    
    // Crear buffer
    let _buffer_id = client.create_buffer(_surface_id, 800, 600, BufferFormat::XRGB8888)?;
    
    // Commit cambios
    client.commit_surface(_surface_id)?;
    
    // Procesar eventos
    client.process_events()?;
    
    // Obtener estadísticas
    let _stats = client.get_stats();
    
    // Desconectar
    client.disconnect();
    
    Ok(())
}

/// Ejemplo de aplicación Wayland
pub fn run_wayland_apps_example() -> Result<(), &'static str> {
    // Crear gestor de aplicaciones
    let mut app_manager = WaylandAppManager::new();
    
    // Inicializar gestor
    app_manager.initialize()?;
    
    // Crear aplicaciones
    app_manager.create_terminal()?;
    app_manager.create_calculator()?;
    app_manager.create_clock()?;
    
    // Ejecutar aplicaciones
    app_manager.run()?;
    
    // Obtener estadísticas
    let _stats = app_manager.get_stats();
    
    // Detener gestor
    app_manager.stop();
    
    Ok(())
}

/// Ejemplo de sistema de renderización
pub fn run_rendering_example() -> Result<(), &'static str> {
    // Crear renderizador con diferentes backends
    let backends = [
        RenderBackend::Software,
        RenderBackend::OpenGL,
        RenderBackend::Vulkan,
        RenderBackend::DirectFB,
    ];
    
    for backend in &backends {
        let mut renderer = WaylandRenderer::new(backend.clone());
        
        // Inicializar renderizador
        match renderer.initialize() {
            Ok(_) => {
                // Obtener estadísticas
                let _stats = renderer.get_stats();
                
                // Simular renderizado
                renderer.render_frame()?;
            }
            Err(_e) => {
                // Error inicializando backend
            }
        }
    }
    
    Ok(())
}

/// Ejemplo completo del sistema Wayland
pub fn run_complete_wayland_example() -> Result<(), &'static str> {
    // 1. Probar sistema de renderización
    run_rendering_example()?;
    
    // 2. Probar servidor Wayland
    run_wayland_server_example()?;
    
    // 3. Probar cliente Wayland
    run_wayland_client_example()?;
    
    // 4. Probar aplicaciones Wayland
    run_wayland_apps_example()?;
    
    Ok(())
}

/// Función de demostración para mostrar capacidades
pub fn demonstrate_wayland_capabilities() -> Result<(), &'static str> {
    // Mostrar información del sistema
    // En un sistema real, aquí se mostrarían las capacidades
    // Por ahora, solo retornamos éxito
    
    Ok(())
}