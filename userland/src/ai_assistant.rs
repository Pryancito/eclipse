//! AI Assistant Module
//! Asistente inteligente del sistema

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Estructura interna del asistente
struct AssistantInternal {
    knowledge_base: HashMap<String, String>,
    task_history: Vec<String>,
    recommendations: Vec<String>,
}

/// Handle de asistente
pub struct AssistantHandle {
    internal: Arc<Mutex<AssistantInternal>>,
}

impl Default for AssistantHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl AssistantHandle {
    fn new() -> Self {
        let mut knowledge_base = HashMap::new();
        
        // Base de conocimiento inicial
        knowledge_base.insert("filesystem".to_string(), 
            "Para operaciones de archivos, usa los comandos: ls, cat, cp, mv, rm".to_string());
        knowledge_base.insert("network".to_string(), 
            "Para redes, verifica la configuración con ifconfig y ping".to_string());
        knowledge_base.insert("memory".to_string(), 
            "Para memoria, usa el comando free para ver uso de memoria".to_string());
        knowledge_base.insert("process".to_string(), 
            "Para procesos, usa ps para listar y kill para terminar".to_string());
        knowledge_base.insert("performance".to_string(), 
            "Para rendimiento, monitorea con top y revisa logs del sistema".to_string());
        
        AssistantHandle {
            internal: Arc::new(Mutex::new(AssistantInternal {
                knowledge_base,
                task_history: Vec::new(),
                recommendations: Vec::new(),
            })),
        }
    }
}

/// Inicializar asistente del sistema
pub fn SystemAssistant_Initialize() {
    println!("🤖 Asistente del sistema inicializado");
}

/// Crear asistente
pub fn create_system_assistant() -> AssistantHandle {
    AssistantHandle::new()
}

/// Proporcionar ayuda
pub fn provide_help(assistant: &AssistantHandle, query: &str) -> String {
    if let Ok(internal) = assistant.internal.lock() {
        let query_lower = query.to_lowercase();
        
        // Buscar en la base de conocimiento
        for (topic, help) in internal.knowledge_base.iter() {
            if query_lower.contains(topic) {
                return format!("📚 Ayuda sobre '{}': {}", topic, help);
            }
        }
        
        // Ayuda general si no se encuentra un tema específico
        if query_lower.contains("help") || query_lower.contains("ayuda") {
            return "🤖 Temas disponibles: filesystem, network, memory, process, performance. \
                    Pregunta sobre un tema específico para obtener ayuda.".to_string();
        }
        
        format!("🤔 No se encontró ayuda específica para '{}'. \
                 Prueba preguntar sobre: filesystem, network, memory, process, o performance.", query)
    } else {
        "❌ Error accediendo al asistente".to_string()
    }
}

/// Solucionar problemas
pub fn troubleshoot(assistant: &AssistantHandle, issue: &str) -> String {
    if let Ok(mut internal) = assistant.internal.lock() {
        let issue_lower = issue.to_lowercase();
        
        // Registrar el problema en el historial
        internal.task_history.push(format!("Troubleshoot: {}", issue));
        
        // Análisis simple de problemas comunes
        if issue_lower.contains("slow") || issue_lower.contains("lento") {
            return "🔧 Problema de rendimiento detectado:\n\
                    1. Verifica uso de CPU y memoria con 'top'\n\
                    2. Revisa procesos en segundo plano\n\
                    3. Limpia archivos temporales\n\
                    4. Considera reiniciar servicios no esenciales".to_string();
        }
        
        if issue_lower.contains("crash") || issue_lower.contains("error") {
            return "🔧 Problema de estabilidad detectado:\n\
                    1. Revisa los logs del sistema\n\
                    2. Verifica espacio en disco\n\
                    3. Comprueba permisos de archivos\n\
                    4. Considera restaurar desde un snapshot reciente".to_string();
        }
        
        if issue_lower.contains("network") || issue_lower.contains("red") {
            return "🔧 Problema de red detectado:\n\
                    1. Verifica cables y conexiones físicas\n\
                    2. Prueba conectividad con ping\n\
                    3. Revisa configuración de firewall\n\
                    4. Reinicia interfaz de red".to_string();
        }
        
        if issue_lower.contains("disk") || issue_lower.contains("disco") {
            return "🔧 Problema de almacenamiento detectado:\n\
                    1. Verifica espacio disponible con df\n\
                    2. Ejecuta fsck para verificar integridad\n\
                    3. Revisa logs de errores de disco\n\
                    4. Considera desfragmentación si es necesario".to_string();
        }
        
        format!("🔧 Análisis del problema '{}':\n\
                 No se identificó un problema específico conocido.\n\
                 Recomendaciones generales:\n\
                 1. Revisa los logs del sistema\n\
                 2. Verifica recursos del sistema (CPU, memoria, disco)\n\
                 3. Busca mensajes de error relacionados", issue)
    } else {
        "❌ Error accediendo al asistente".to_string()
    }
}

/// Automatizar tarea
pub fn automate_task(assistant: &AssistantHandle, task: &str) -> bool {
    if let Ok(mut internal) = assistant.internal.lock() {
        let task_lower = task.to_lowercase();
        
        // Registrar tarea en el historial
        internal.task_history.push(format!("Automate: {}", task));
        
        // Simular automatización de tareas comunes
        if task_lower.contains("backup") {
            println!("🔄 Automatizando backup del sistema...");
            println!("   - Identificando archivos críticos");
            println!("   - Creando snapshot");
            println!("   - Programando backups automáticos");
            return true;
        }
        
        if task_lower.contains("update") || task_lower.contains("actualizar") {
            println!("🔄 Automatizando actualizaciones...");
            println!("   - Verificando actualizaciones disponibles");
            println!("   - Descargando paquetes");
            println!("   - Programando instalación");
            return true;
        }
        
        if task_lower.contains("clean") || task_lower.contains("limpiar") {
            println!("🔄 Automatizando limpieza del sistema...");
            println!("   - Eliminando archivos temporales");
            println!("   - Limpiando caché");
            println!("   - Optimizando almacenamiento");
            return true;
        }
        
        println!("🤖 Tarea '{}' agregada a la cola de automatización", task);
        true
    } else {
        false
    }
}

/// Proporcionar recomendaciones
pub fn provide_recommendations(assistant: &AssistantHandle) -> Vec<String> {
    if let Ok(mut internal) = assistant.internal.lock() {
        // Generar recomendaciones basadas en historial
        let mut recommendations = vec![
            "🔒 Considera habilitar encriptación de archivos sensibles".to_string(),
            "📊 Revisa el uso de memoria periódicamente".to_string(),
            "🔄 Programa backups automáticos regulares".to_string(),
            "🚀 Actualiza el sistema para obtener mejoras de rendimiento".to_string(),
            "🧹 Limpia archivos temporales para liberar espacio".to_string(),
        ];
        
        // Agregar recomendaciones basadas en tareas recientes
        if internal.task_history.iter().any(|t| t.contains("slow")) {
            recommendations.push("⚡ El sistema ha mostrado problemas de rendimiento. \
                                Considera optimizar procesos en segundo plano".to_string());
        }
        
        if internal.task_history.iter().any(|t| t.contains("disk")) {
            recommendations.push("💾 Se han detectado problemas de disco. \
                                Verifica la salud del almacenamiento".to_string());
        }
        
        internal.recommendations = recommendations.clone();
        recommendations
    } else {
        vec![]
    }
}

/// Obtener historial de tareas
pub fn get_task_history(assistant: &AssistantHandle) -> Vec<String> {
    if let Ok(internal) = assistant.internal.lock() {
        internal.task_history.clone()
    } else {
        vec![]
    }
}

/// Agregar conocimiento a la base
pub fn add_knowledge(assistant: &mut AssistantHandle, topic: &str, info: &str) -> bool {
    if let Ok(mut internal) = assistant.internal.lock() {
        internal.knowledge_base.insert(topic.to_string(), info.to_string());
        true
    } else {
        false
    }
}

/// Limpiar historial
pub fn clear_history(assistant: &mut AssistantHandle) -> bool {
    if let Ok(mut internal) = assistant.internal.lock() {
        internal.task_history.clear();
        internal.recommendations.clear();
        true
    } else {
        false
    }
}

/// Liberar asistente
pub fn free_system_assistant(_assistant: &mut AssistantHandle) -> bool {
    // En Rust, se libera automáticamente
    true
}