use anyhow::Result;
use graphics_module::GraphicsDriver;

fn main() -> Result<()> {
    println!("🎨 Eclipse OS Graphics Module v0.6.0");
    println!("=====================================");
    
    // Crear driver gráfico
    let mut driver = GraphicsDriver::new();
    
    // Configurar modo gráfico
    driver.set_mode(1920, 1080, 32)?;
    println!("✅ Modo gráfico establecido: 1920x1080 @ 32bpp");
    
    // Limpiar pantalla con color azul
    driver.clear_screen(0x000080)?;
    println!("✅ Pantalla limpiada con color azul");
    
    // Dibujar algunos elementos de prueba
    driver.draw_rect(100, 100, 200, 100, 0x00FF00)?; // Rectángulo verde
    driver.draw_text(120, 150, "ECLIPSE OS", 0xFFFFFF)?; // Texto blanco
    driver.draw_text(120, 170, "GRAPHICS MODULE", 0xFFFFFF)?; // Texto blanco
    
    // Intercambiar buffers
    driver.swap_buffers()?;
    println!("✅ Buffers intercambiados");
    
    // Simular blitting de buffer SHM
    let test_shm_data = vec![0xFF; 1920 * 1080 * 4]; // Datos de prueba
    driver.blit_shm_buffer(&test_shm_data, 1920, 1080, 1920 * 4)?;
    println!("✅ Buffer SHM simulado blitteado");
    
    // Mostrar información del driver
    let (width, height) = driver.get_dimensions();
    println!("📊 Dimensiones del framebuffer: {}x{}", width, height);
    println!("🔧 Driver inicializado: {}", driver.is_initialized());
    
    println!("\n🎉 Graphics Module funcionando correctamente!");
    
    Ok(())
}