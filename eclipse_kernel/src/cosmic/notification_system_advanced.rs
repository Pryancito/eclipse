use crate::drivers::framebuffer::{Color, FramebufferDriver};
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// Sistema de notificaciones inspirado en COSMIC Epoch
pub struct NotificationSystem {
    pub notifications: Vec<Notification>,
    pub next_notification_id: u32,
}

/// Notificación individual
#[derive(Debug, Clone)]
pub struct Notification {
    pub id: u32,
    pub title: String,
    pub body: String,
    pub urgency: NotificationUrgency,
    pub visible: bool,
}

/// Urgencia de la notificación
#[derive(Debug, Clone, PartialEq)]
pub enum NotificationUrgency {
    Low,
    Normal,
    High,
    Critical,
}

impl NotificationSystem {
    /// Crear nuevo sistema de notificaciones
    pub fn new() -> Self {
        Self {
            notifications: Vec::new(),
            next_notification_id: 1,
        }
    }

    /// Mostrar notificación
    pub fn show_notification(
        &mut self,
        title: String,
        body: String,
        urgency: NotificationUrgency,
    ) -> u32 {
        let notification = Notification {
            id: self.next_notification_id,
            title,
            body,
            urgency,
            visible: true,
        };

        self.notifications.push(notification);
        self.next_notification_id += 1;
        self.next_notification_id - 1
    }

    /// Renderizar notificaciones
    pub fn render_notifications(&self, fb: &mut FramebufferDriver) -> Result<(), String> {
        let mut y_offset = 50;

        for notification in &self.notifications {
            if notification.visible {
                self.render_notification(fb, notification, y_offset)?;
                y_offset += 120;
            }
        }
        Ok(())
    }

    /// Renderizar notificación individual
    fn render_notification(
        &self,
        fb: &mut FramebufferDriver,
        notification: &Notification,
        y_offset: u32,
    ) -> Result<(), String> {
        let x = 50;
        let y = y_offset;
        let width = 350;
        let height = 100;

        // Color según urgencia
        let color = match notification.urgency {
            NotificationUrgency::Low => Color::BLUE,
            NotificationUrgency::Normal => Color::GREEN,
            NotificationUrgency::High => Color::YELLOW,
            NotificationUrgency::Critical => Color::RED,
        };

        // Dibujar fondo
        for current_y in y..(y + height) {
            for current_x in x..(x + width) {
                fb.put_pixel(current_x, current_y, Color::DARK_GRAY);
            }
        }

        // Dibujar borde
        for current_x in x..(x + width) {
            fb.put_pixel(current_x, y, color);
            fb.put_pixel(current_x, y + height - 1, color);
        }
        for current_y in y..(y + height) {
            fb.put_pixel(x, current_y, color);
            fb.put_pixel(x + width - 1, current_y, color);
        }

        // Dibujar texto
        fb.write_text_kernel(&notification.title, Color::WHITE);

        Ok(())
    }
}
