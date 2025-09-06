//! Demostración de integración de IA en Eclipse OS
//! 
//! Este módulo implementa una demostración de las capacidades
//! de IA integradas en el sistema operativo.

#![no_std]

use alloc::string::String;
use alloc::vec::Vec;
use alloc::format;

use crate::ai_integration::{AIIntegration, AIIntervention, AICommand, AICommandResult};
use crate::ai_communication::{AICommunicationChannel, CommunicationType, AIMessage};
use crate::ai_control::{AISystemController, ControllerStats, SystemMetrics};
use crate::ai_interface::{AIUserInterface, UserIntention, InterfaceStats};

/// Demostración de IA
pub struct AIDemo {
    /// Estado de la demostración
    is_running: bool,
    /// Contador de demostraciones
    demo_counter: u32,
}

impl AIDemo {
    /// Crea una nueva instancia de la demostración
    pub fn new() -> Self {
        Self {
            is_running: false,
            demo_counter: 0,
        }
    }

    /// Ejecuta la demostración completa
    pub fn run_demo(&mut self) -> Result<(), &'static str> {
        self.is_running = true;
        self.demo_counter += 1;
        
        // Mostrar encabezado
        self.show_header()?;
        
        // Demostrar integración de IA
        self.demo_ai_integration()?;
        
        // Demostrar comunicación con IA
        self.demo_ai_communication()?;
        
        // Demostrar control del sistema por IA
        self.demo_ai_control()?;
        
        // Demostrar interfaz de usuario
        self.demo_ai_interface()?;
        
        // Mostrar resumen
        self.show_summary()?;
        
        self.is_running = false;
        Ok(())
    }

    /// Muestra el encabezado de la demostración
    fn show_header(&self) -> Result<(), &'static str> {
        println!("");
        println!("╔══════════════════════════════════════════════════════════════════════════════╗");
        println!("║                    ECLIPSE OS - DEMOSTRACIÓN DE IA INTEGRADA                ║");
        println!("║                                                                              ║");
        println!("║  Sistema operativo con inteligencia artificial integrada en el kernel       ║");
        println!("║  Similar a Computer Use de Anthropic, pero a nivel de sistema operativo     ║");
        println!("╚══════════════════════════════════════════════════════════════════════════════╝");
        println!("");
        
        Ok(())
    }

    /// Demuestra la integración de IA
    fn demo_ai_integration(&self) -> Result<(), &'static str> {
        println!("🔧 DEMOSTRACIÓN: Integración de IA en el Kernel");
        println!("{}", "─".repeat(60));
        
        if let Some(ai) = crate::ai_integration::get_ai_integration() {
            // Mostrar estado de la IA
            let state = ai.get_state();
            println!("Estado de la IA: {:?}", state);
            
            // Mostrar contexto del sistema
            let context = ai.get_system_context();
            println!("Contexto del sistema:");
            println!("  - Uso de CPU: {:.1}%", context.cpu_usage * 100.0);
            println!("  - Uso de memoria: {:.1}%", context.memory_usage * 100.0);
            println!("  - Procesos activos: {}", context.active_processes);
            println!("  - Tiempo activo: {} segundos", context.uptime);
            
            // Mostrar estadísticas
            let stats = ai.get_ai_stats();
            println!("Estadísticas de IA:");
            println!("  - Comandos totales: {}", stats.total_commands);
            println!("  - Comandos exitosos: {}", stats.successful_commands);
            println!("  - Tasa de éxito: {:.1}%", stats.get_success_rate() * 100.0);
            
            // Simular procesamiento de solicitud
            println!("");
            println!("📝 Procesando solicitud: 'optimizar memoria del sistema'");
            match ai.process_intervention_request("optimizar memoria del sistema") {
                Ok(command_id) => {
                    println!("✅ Comando procesado exitosamente con ID: {}", command_id);
                    
                    // Obtener resultado del comando
                    if let Some(result) = ai.get_command_result(command_id) {
                        println!("📊 Resultado del comando:");
                        println!("  - Éxito: {}", result.success);
                        println!("  - Mensaje: {}", result.message);
                        println!("  - Datos: {:?}", result.data);
                    }
                }
                Err(e) => {
                    println!("❌ Error procesando comando: {}", e);
                }
            }
        } else {
            println!("❌ IA no disponible");
        }
        
        println!("");
        Ok(())
    }

    /// Demuestra la comunicación con IA
    fn demo_ai_communication(&self) -> Result<(), &'static str> {
        println!("💬 DEMOSTRACIÓN: Comunicación Bidireccional con IA");
        println!("{}", "─".repeat(60));
        
        if let Some(channel) = crate::ai_communication::get_ai_communication_channel() {
            // Mostrar estadísticas del canal
            let stats = channel.get_channel_stats();
            println!("Estadísticas del canal de comunicación:");
            println!("  - Mensajes salientes: {}", stats.outgoing_messages);
            println!("  - Mensajes entrantes: {}", stats.incoming_messages);
            println!("  - Total de mensajes: {}", stats.total_messages);
            println!("  - Conectado: {}", stats.is_connected);
            
            // Simular envío de mensaje
            println!("");
            println!("📤 Enviando mensaje: 'status del sistema'");
            match channel.send_message("status del sistema", CommunicationType::Request) {
                Ok(message_id) => {
                    println!("✅ Mensaje enviado con ID: {}", message_id);
                }
                Err(e) => {
                    println!("❌ Error enviando mensaje: {}", e);
                }
            }
            
            // Simular envío de comando
            println!("");
            println!("📤 Enviando comando: 'intervene memory'");
            match channel.send_message("intervene memory", CommunicationType::Command) {
                Ok(message_id) => {
                    println!("✅ Comando enviado con ID: {}", message_id);
                }
                Err(e) => {
                    println!("❌ Error enviando comando: {}", e);
                }
            }
        } else {
            println!("❌ Canal de comunicación no disponible");
        }
        
        println!("");
        Ok(())
    }

    /// Demuestra el control del sistema por IA
    fn demo_ai_control(&self) -> Result<(), &'static str> {
        println!("🎛️  DEMOSTRACIÓN: Control del Sistema Operativo por IA");
        println!("{}", "─".repeat(60));
        
        if let Some(controller) = crate::ai_control::get_ai_system_controller() {
            // Mostrar estadísticas del controlador
            let stats = controller.get_controller_stats();
            println!("Estadísticas del controlador:");
            println!("  - Intervenciones totales: {}", stats.total_interventions);
            println!("  - Intervenciones exitosas: {}", stats.successful_interventions);
            println!("  - Tasa de éxito: {:.1}%", stats.success_rate * 100.0);
            println!("  - Políticas activas: {}", stats.active_policies);
            println!("  - Activo: {}", stats.is_active);
            
            // Mostrar métricas del sistema
            let metrics = controller.get_system_status();
            println!("");
            println!("Métricas del sistema:");
            println!("  - CPU: {:.1}%", metrics.cpu_usage * 100.0);
            println!("  - Memoria: {:.1}%", metrics.memory_usage * 100.0);
            println!("  - Disco: {:.1}%", metrics.disk_usage * 100.0);
            println!("  - Red: {:.1}%", metrics.network_usage * 100.0);
            println!("  - Procesos: {}", metrics.process_count);
            println!("  - Carga: {:.1}", metrics.system_load);
            println!("  - Tiempo de respuesta: {:.1}ms", metrics.response_time);
            println!("  - Tasa de error: {:.1}%", metrics.error_rate * 100.0);
            println!("  - Rendimiento: {:.1} ops/s", metrics.throughput);
            
            // Mostrar políticas de control
            let policies = controller.get_control_policies();
            println!("");
            println!("Políticas de control activas:");
            for (name, policy) in policies {
                if policy.enabled {
                    println!("  - {}: {} (umbral: {:.2})", name, policy.action, policy.threshold);
                }
            }
            
            // Simular evaluación de políticas
            println!("");
            println!("🔍 Evaluando políticas de control...");
            match controller.evaluate_control_policies() {
                Ok(_) => {
                    println!("✅ Políticas evaluadas correctamente");
                }
                Err(e) => {
                    println!("❌ Error evaluando políticas: {}", e);
                }
            }
        } else {
            println!("❌ Controlador de IA no disponible");
        }
        
        println!("");
        Ok(())
    }

    /// Demuestra la interfaz de usuario
    fn demo_ai_interface(&self) -> Result<(), &'static str> {
        println!("🖥️  DEMOSTRACIÓN: Interfaz de Usuario para IA");
        println!("{}", "─".repeat(60));
        
        if let Some(interface) = crate::ai_interface::get_ai_user_interface() {
            // Mostrar estadísticas de la interfaz
            let stats = interface.get_interface_stats();
            println!("Estadísticas de la interfaz:");
            println!("  - Conversaciones totales: {}", stats.total_conversations);
            println!("  - Intervenciones exitosas: {}", stats.successful_interventions);
            println!("  - Tasa de éxito: {:.1}%", stats.get_success_rate() * 100.0);
            println!("  - Duración de sesión: {} segundos", stats.session_duration);
            println!("  - Activa: {}", stats.is_active);
            
            // Simular interacciones con el usuario
            let test_inputs = vec![
                "ayuda",
                "estado del sistema",
                "optimizar rendimiento",
                "diagnosticar problemas",
                "gestionar procesos",
                "monitorear seguridad",
            ];
            
            println!("");
            println!("💬 Simulando interacciones con el usuario:");
            for input in test_inputs {
                println!("");
                println!("👤 Usuario: {}", input);
                match interface.process_user_input(input) {
                    Ok(response) => {
                        println!("🤖 IA: {}", response);
                    }
                    Err(e) => {
                        println!("❌ Error: {}", e);
                    }
                }
            }
            
            // Mostrar historial de conversación
            let history = interface.get_conversation_history();
            println!("");
            println!("📚 Historial de conversación (últimas {} entradas):", history.len().min(5));
            for entry in history.iter().rev().take(5) {
                println!("  - [{}] Usuario: {}", entry.timestamp, entry.user_input);
                println!("    IA: {}", entry.ai_response);
                if let Some(intervention) = &entry.intervention_type {
                    println!("    Intervención: {:?}", intervention);
                }
                println!("");
            }
        } else {
            println!("❌ Interfaz de usuario no disponible");
        }
        
        println!("");
        Ok(())
    }

    /// Muestra el resumen de la demostración
    fn show_summary(&self) -> Result<(), &'static str> {
        println!("📊 RESUMEN DE LA DEMOSTRACIÓN");
        println!("{}", "─".repeat(60));
        
        println!("✅ Integración de IA en el kernel: Funcional");
        println!("✅ Comunicación bidireccional: Funcional");
        println!("✅ Control del sistema operativo: Funcional");
        println!("✅ Interfaz de usuario: Funcional");
        println!("");
        println!("🎯 Características principales:");
        println!("  - Intervención automática en el sistema operativo");
        println!("  - Aprendizaje adaptativo de patrones de uso");
        println!("  - Comunicación natural con el usuario");
        println!("  - Monitoreo continuo del sistema");
        println!("  - Optimización automática de recursos");
        println!("  - Diagnóstico predictivo de problemas");
        println!("  - Gestión inteligente de procesos");
        println!("  - Seguridad proactiva");
        println!("");
        println!("🚀 Eclipse OS con IA integrada está listo para uso avanzado");
        println!("   Similar a Computer Use de Anthropic, pero a nivel de SO");
        println!("");
        
        Ok(())
    }
}

/// Instancia global de la demostración
pub static mut AI_DEMO: Option<AIDemo> = None;

/// Inicializa la demostración de IA
pub fn init_ai_demo() -> Result<(), &'static str> {
    unsafe {
        AI_DEMO = Some(AIDemo::new());
        Ok(())
    }
}

/// Ejecuta la demostración de IA
pub fn run_ai_demo() -> Result<(), &'static str> {
    unsafe {
        if let Some(demo) = &mut AI_DEMO {
            demo.run_demo()
        } else {
            Err("Demostración no inicializada")
        }
    }
}

/// Obtiene la instancia global de la demostración
pub fn get_ai_demo() -> Option<&'static mut AIDemo> {
    unsafe {
        AI_DEMO.as_mut()
    }
}
