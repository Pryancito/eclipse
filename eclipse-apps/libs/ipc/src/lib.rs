//! Biblioteca de Comunicación Inter-Proceso para Eclipse OS
//!
//! Esta biblioteca proporciona funcionalidades para comunicación
//! entre procesos en el sistema Eclipse OS, incluyendo:
//!
//! - Sockets Unix para comunicación local
//! - Mensajes serializados con bincode
//! - Comunicación asíncrona con tokio
//! - API de alto nivel para envío/recepción de mensajes

use serde::{Deserialize, Serialize};
use tokio::net::UnixStream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use bincode;
use std::path::Path;
use std::time::Duration;

/// Tipos de mensajes IPC para systemd
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IpcMessage {
    /// Servicio listo para funcionar
    Ready { service: String },
    /// Estado de salud del servicio
    Health { service: String, ok: bool },
    /// Respuesta a ping
    Pong { service: String },
    /// Ping para verificar conectividad
    Ping { service: String },
}

/// Cliente IPC para comunicación con otros procesos
pub struct UnixBus {
    stream: UnixStream,
}

impl UnixBus {
    /// Conectar a un socket IPC con reintentos
    pub async fn connect_with_retry<P: AsRef<Path>>(
        socket_path: P,
        max_retries: u32,
        delay: Duration
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let mut last_error = None;

        for _ in 0..max_retries {
            match UnixStream::connect(&socket_path).await {
                Ok(stream) => return Ok(Self { stream }),
                Err(e) => {
                    last_error = Some(e);
                    tokio::time::sleep(delay).await;
                }
            }
        }

        Err(Box::new(last_error.unwrap_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "Connection failed"))))
    }

    /// Recibir un mensaje con timeout
    pub async fn recv_timeout<T>(
        &mut self,
        buf: &mut Vec<u8>,
        timeout: Duration
    ) -> Result<Option<T>, Box<dyn std::error::Error>>
    where
        T: for<'de> Deserialize<'de>,
    {
        // Para simplificar, por ahora no implementamos timeout real
        // Leer longitud
        let mut len_buf = [0u8; 4];
        match tokio::time::timeout(timeout, self.stream.read_exact(&mut len_buf)).await {
            Ok(Ok(_)) => {
                let len = u32::from_le_bytes(len_buf) as usize;

                // Leer datos
                buf.resize(len, 0);
                self.stream.read_exact(buf).await?;

                // Deserializar mensaje
                let message: T = bincode::deserialize(buf)?;
                Ok(Some(message))
            }
            _ => Ok(None), // Timeout o error
        }
    }

    /// Enviar un mensaje
    pub async fn send<T>(&mut self, message: &T) -> Result<(), Box<dyn std::error::Error>>
    where
        T: Serialize,
    {
        let data = bincode::serialize(message)?;
        let len = data.len() as u32;

        // Enviar longitud primero
        self.stream.write_all(&len.to_le_bytes()).await?;
        // Enviar datos
        self.stream.write_all(&data).await?;

        Ok(())
    }
}

/// Obtener el path del socket IPC desde variables de entorno
pub fn socket_path_from_env() -> String {
    std::env::var("ECLIPSE_IPC_SOCKET")
        .unwrap_or_else(|_| "/tmp/eclipse_systemd.sock".to_string())
}

/// Tests básicos
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ipc_message_serialization() {
        let message = IpcMessage::Ready {
            service: "test_service".to_string()
        };

        let serialized = bincode::serialize(&message).unwrap();
        let deserialized: IpcMessage = bincode::deserialize(&serialized).unwrap();

        match deserialized {
            IpcMessage::Ready { service } => assert_eq!(service, "test_service"),
            _ => panic!("Wrong message type"),
        }
    }

    #[test]
    fn test_socket_path_from_env() {
        // Sin variable de entorno, debería usar el valor por defecto
        std::env::remove_var("ECLIPSE_IPC_SOCKET");
        let path = socket_path_from_env();
        assert_eq!(path, "/tmp/eclipse_systemd.sock");

        // Con variable de entorno
        std::env::set_var("ECLIPSE_IPC_SOCKET", "/custom/path.sock");
        let path = socket_path_from_env();
        assert_eq!(path, "/custom/path.sock");

        // Limpiar
        std::env::remove_var("ECLIPSE_IPC_SOCKET");
    }
}
