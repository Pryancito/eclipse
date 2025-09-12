//! Sistema de notificaciones para Eclipse SystemD
//!
//! Este módulo implementa el sistema de notificaciones de systemd
//! que permite a los servicios enviar señales de estado.

use anyhow::Result;
use log::{info, warn, debug};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;
use chrono::{DateTime, Utc};

/// Tipo de notificación
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum NotificationType {
    /// Servicio listo
    Ready,
    /// Servicio recargando
    Reloading,
    /// Servicio deteniéndose
    Stopping,
    /// Error en el servicio
    Error,
    /// Estado personalizado
    Custom(String),
}

/// Notificación de servicio
#[derive(Debug, Clone)]
pub struct ServiceNotification {
    pub service_name: String,
    pub notification_type: NotificationType,
    pub message: String,
    pub timestamp: DateTime<Utc>,
    pub pid: Option<u32>,
    pub status_code: Option<i32>,
    pub fields: HashMap<String, String>,
}

/// Canal de notificaciones
#[derive(Debug)]
pub struct NotificationChannel {
    /// Nombre del canal
    name: String,
    /// Canal de broadcast
    sender: broadcast::Sender<ServiceNotification>,
    /// Suscriptores activos
    subscribers: Arc<Mutex<HashMap<String, broadcast::Receiver<ServiceNotification>>>>,
}

/// Manager de notificaciones
pub struct NotificationManager {
    /// Canales de notificaciones
    channels: Arc<Mutex<HashMap<String, NotificationChannel>>>,
    /// Historial de notificaciones
    history: Arc<Mutex<Vec<ServiceNotification>>>,
    /// Tamaño máximo del historial
    max_history_size: usize,
}

impl NotificationManager {
    /// Crea una nueva instancia del manager de notificaciones
    pub fn new() -> Self {
        Self {
            channels: Arc::new(Mutex::new(HashMap::new())),
            history: Arc::new(Mutex::new(Vec::new())),
            max_history_size: 1000,
        }
    }

    /// Crea un nuevo canal de notificaciones
    pub fn create_channel(&self, name: &str, capacity: usize) -> Result<()> {
        let (sender, _) = broadcast::channel(capacity);

        let channel = NotificationChannel {
            name: name.to_string(),
            sender,
            subscribers: Arc::new(Mutex::new(HashMap::new())),
        };

        self.channels.lock().unwrap().insert(name.to_string(), channel);
        info!("Notificacion Canal de notificaciones creado: {}", name);
        Ok(())
    }

    /// Envía una notificación
    pub fn send_notification(&self, channel_name: &str, notification: ServiceNotification) -> Result<()> {
        let channels = self.channels.lock().unwrap();

        if let Some(channel) = channels.get(channel_name) {
            // Enviar notificación a todos los suscriptores
            let _ = channel.sender.send(notification.clone());

            // Agregar al historial
            self.add_to_history(notification.clone());

            debug!("Enviando Notificación enviada en canal {}: {:?}", channel_name, notification.notification_type);
        } else {
            warn!("Advertencia  Canal de notificaciones no encontrado: {}", channel_name);
        }

        Ok(())
    }

    /// Suscribe a un canal de notificaciones
    pub fn subscribe(&self, channel_name: &str, subscriber_id: &str) -> Result<broadcast::Receiver<ServiceNotification>> {
        let channels = self.channels.lock().unwrap();

        if let Some(channel) = channels.get(channel_name) {
            let receiver = channel.sender.subscribe();
            channel.subscribers.lock().unwrap().insert(subscriber_id.to_string(), channel.sender.subscribe());
            debug!("Suscriptor Suscriptor {} agregado al canal {}", subscriber_id, channel_name);
            Ok(receiver)
        } else {
            Err(anyhow::anyhow!("Canal no encontrado: {}", channel_name))
        }
    }

    /// Cancela la suscripción de un canal
    pub fn unsubscribe(&self, channel_name: &str, subscriber_id: &str) -> Result<()> {
        let channels = self.channels.lock().unwrap();

        if let Some(channel) = channels.get(channel_name) {
            channel.subscribers.lock().unwrap().remove(subscriber_id);
            debug!("🚫 Suscriptor {} removido del canal {}", subscriber_id, channel_name);
            Ok(())
        } else {
            Err(anyhow::anyhow!("Canal no encontrado: {}", channel_name))
        }
    }

    /// Notifica que un servicio está listo
    pub fn notify_service_ready(&self, service_name: &str, pid: u32) -> Result<()> {
        let notification = ServiceNotification {
            service_name: service_name.to_string(),
            notification_type: NotificationType::Ready,
            message: format!("Servicio {} está listo", service_name),
            timestamp: Utc::now(),
            pid: Some(pid),
            status_code: Some(0),
            fields: HashMap::new(),
        };

        self.send_notification("systemd", notification)
    }

    /// Notifica que un servicio se está recargando
    pub fn notify_service_reloading(&self, service_name: &str, pid: u32) -> Result<()> {
        let notification = ServiceNotification {
            service_name: service_name.to_string(),
            notification_type: NotificationType::Reloading,
            message: format!("Servicio {} se está recargando", service_name),
            timestamp: Utc::now(),
            pid: Some(pid),
            status_code: None,
            fields: HashMap::new(),
        };

        self.send_notification("systemd", notification)
    }

    /// Notifica que un servicio se está deteniendo
    pub fn notify_service_stopping(&self, service_name: &str, pid: Option<u32>) -> Result<()> {
        let notification = ServiceNotification {
            service_name: service_name.to_string(),
            notification_type: NotificationType::Stopping,
            message: format!("Servicio {} se está deteniendo", service_name),
            timestamp: Utc::now(),
            pid,
            status_code: None,
            fields: HashMap::new(),
        };

        self.send_notification("systemd", notification)
    }

    /// Notifica un error en un servicio
    pub fn notify_service_error(&self, service_name: &str, error: &str, pid: Option<u32>) -> Result<()> {
        let mut fields = HashMap::new();
        fields.insert("ERROR".to_string(), error.to_string());

        let notification = ServiceNotification {
            service_name: service_name.to_string(),
            notification_type: NotificationType::Error,
            message: format!("Error en servicio {}: {}", service_name, error),
            timestamp: Utc::now(),
            pid,
            status_code: Some(-1),
            fields,
        };

        self.send_notification("systemd", notification)
    }

    /// Obtiene el historial de notificaciones
    pub fn get_notification_history(&self, limit: Option<usize>) -> Vec<ServiceNotification> {
        let history = self.history.lock().unwrap();
        let limit = limit.unwrap_or(history.len());
        history.iter().rev().take(limit).cloned().collect()
    }

    /// Obtiene estadísticas de notificaciones
    pub fn get_notification_stats(&self) -> NotificationStats {
        let history = self.history.lock().unwrap();

        let mut type_counts = HashMap::new();
        let mut service_counts = HashMap::new();

        for notification in history.iter() {
            *type_counts.entry(notification.notification_type.clone()).or_insert(0) += 1;
            *service_counts.entry(notification.service_name.clone()).or_insert(0) += 1;
        }

        NotificationStats {
            total_notifications: history.len(),
            type_counts,
            service_counts,
        }
    }

    /// Agrega una notificación al historial
    fn add_to_history(&self, notification: ServiceNotification) {
        let mut history = self.history.lock().unwrap();

        history.push(notification);

        // Mantener tamaño máximo del historial
        if history.len() > self.max_history_size {
            let excess = history.len() - self.max_history_size;
            history.drain(0..excess);
        }
    }
}

/// Estadísticas de notificaciones
#[derive(Debug, Clone)]
pub struct NotificationStats {
    pub total_notifications: usize,
    pub type_counts: HashMap<NotificationType, usize>,
    pub service_counts: HashMap<String, usize>,
}

impl NotificationStats {
    pub fn get_summary(&self) -> String {
        format!(
            "Notificaciones: {} total, {} tipos, {} servicios",
            self.total_notifications,
            self.type_counts.len(),
            self.service_counts.len()
        )
    }
}

impl Default for NotificationType {
    fn default() -> Self {
        NotificationType::Custom("unknown".to_string())
    }
}

/// Macro para enviar notificaciones fácilmente
#[macro_export]
macro_rules! notify_service {
    ($manager:expr, $type:ident, $service:expr) => {
        if let Err(e) = $manager.notify_service_$type($service, None) {
            log::warn!("Error enviando notificación: {}", e);
        }
    };
    ($manager:expr, $type:ident, $service:expr, $pid:expr) => {
        if let Err(e) = $manager.notify_service_$type($service, $pid) {
            log::warn!("Error enviando notificación: {}", e);
        }
    };
}
