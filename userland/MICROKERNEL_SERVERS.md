# Servidores Microkernel en Userspace

Este documento describe la implementación de servidores del sistema en espacio de usuario (userspace) como parte de la arquitectura microkernel de Eclipse OS.

## Descripción General

Los servidores del microkernel son componentes independientes que se ejecutan en espacio de usuario y manejan las funcionalidades del sistema operativo. Se comunican con el kernel a través del sistema de mensajes IPC del microkernel.

## Arquitectura

### Estructura de Archivos

```
userland/src/services/servers/
├── mod.rs                    # Módulo principal y trait MicrokernelServer
├── filesystem_server.rs      # Servidor de sistema de archivos
├── graphics_server.rs        # Servidor de gráficos
├── network_server.rs         # Servidor de red
├── input_server.rs           # Servidor de entrada (teclado/mouse)
├── audio_server.rs           # Servidor de audio
├── ai_server.rs              # Servidor de IA
└── security_server.rs        # Servidor de seguridad
```

### Trait MicrokernelServer

Todos los servidores implementan el trait común `MicrokernelServer`:

```rust
pub trait MicrokernelServer {
    fn name(&self) -> &str;
    fn message_type(&self) -> MessageType;
    fn priority(&self) -> u8;
    fn initialize(&mut self) -> Result<()>;
    fn process_message(&mut self, message: &Message) -> Result<Vec<u8>>;
    fn shutdown(&mut self) -> Result<()>;
    fn get_stats(&self) -> ServerStats;
}
```

## Servidores Implementados

### 1. Security Server (Prioridad: 10 - Máxima)
**Archivo**: `security_server.rs`

Maneja autenticación, autorización, encriptación y auditoría.

**Comandos soportados**:
- `Authenticate`: Autenticar usuario y obtener token de sesión
- `Authorize`: Verificar autorización para acceder a un recurso
- `Encrypt`: Encriptar datos
- `Decrypt`: Desencriptar datos
- `Hash`: Generar hash SHA-256
- `Audit`: Registrar evento de auditoría
- `CheckPermission`: Verificar permisos de usuario

### 2. FileSystem Server (Prioridad: 10 - Alta)
**Archivo**: `filesystem_server.rs`

Gestiona todas las operaciones de I/O de archivos.

**Comandos soportados**:
- `Open`: Abrir archivo y obtener file descriptor
- `Read`: Leer datos de un archivo
- `Write`: Escribir datos a un archivo
- `Close`: Cerrar file descriptor
- `Delete`: Eliminar archivo
- `Create`: Crear nuevo archivo
- `List`: Listar contenido de directorio
- `Stat`: Obtener información de archivo

### 3. Graphics Server (Prioridad: 9 - Alta)
**Archivo**: `graphics_server.rs`

Maneja operaciones de display y renderizado.

**Comandos soportados**:
- `InitDisplay`: Inicializar display con resolución específica
- `DrawPixel`: Dibujar pixel en coordenadas específicas
- `DrawRect`: Dibujar rectángulo
- `DrawLine`: Dibujar línea
- `Clear`: Limpiar pantalla con color
- `Swap`: Intercambiar buffers (double buffering)
- `SetMode`: Cambiar modo de video

### 4. Network Server (Prioridad: 8 - Media-Alta)
**Archivo**: `network_server.rs`

Implementa stack TCP/IP y gestión de red.

**Comandos soportados**:
- `SocketCreate`: Crear nuevo socket
- `Bind`: Asociar socket a puerto
- `Send`: Enviar datos por socket
- `Recv`: Recibir datos de socket

### 5. Input Server (Prioridad: 9 - Alta)
**Archivo**: `input_server.rs`

Maneja eventos de dispositivos de entrada.

**Comandos soportados**:
- `KeyboardEvent`: Procesar evento de teclado
- `MouseEvent`: Procesar evento de mouse
- `GetKeyboardState`: Obtener estado actual del teclado
- `GetMouseState`: Obtener posición y estado del mouse

### 6. Audio Server (Prioridad: 7 - Media)
**Archivo**: `audio_server.rs`

Gestiona reproducción y captura de audio.

**Comandos soportados**:
- `Play`: Reproducir audio
- `Capture`: Capturar audio del micrófono
- `SetVolume`: Configurar volumen
- `GetVolume`: Obtener volumen actual

### 7. AI Server (Prioridad: 6 - Baja)
**Archivo**: `ai_server.rs`

Ejecuta inferencia de modelos de inteligencia artificial.

**Comandos soportados**:
- `Inference`: Ejecutar inferencia con prompt
- `LoadModel`: Cargar modelo de IA
- `UnloadModel`: Descargar modelo de memoria
- `AnomalyDetection`: Detectar anomalías en datos
- `Prediction`: Ejecutar predicción

## Gestor de Servidores

### MicrokernelServerManager

El `MicrokernelServerManager` coordina todos los servidores:

```rust
pub struct MicrokernelServerManager {
    servers: Vec<Box<dyn MicrokernelServer>>,
    running: bool,
}
```

**Funciones principales**:
- `register_server()`: Registrar nuevo servidor
- `initialize_all()`: Inicializar todos los servidores
- `route_message()`: Enrutar mensaje al servidor apropiado
- `get_all_stats()`: Obtener estadísticas de todos los servidores
- `shutdown_all()`: Detener todos los servidores ordenadamente

## Integración con el Sistema

### System Service Manager

Los servidores del microkernel se integran con el `SystemServiceManager`:

```rust
pub fn initialize_microkernel_servers(&mut self) -> Result<()> {
    let mut manager = MicrokernelServerManager::new();
    
    // Registrar en orden de prioridad
    manager.register_server(Box::new(SecurityServer::new()))?;
    manager.register_server(Box::new(FileSystemServer::new()))?;
    manager.register_server(Box::new(GraphicsServer::new()))?;
    manager.register_server(Box::new(NetworkServer::new()))?;
    manager.register_server(Box::new(InputServer::new()))?;
    manager.register_server(Box::new(AudioServer::new()))?;
    manager.register_server(Box::new(AIServer::new()))?;
    
    manager.initialize_all()?;
    Ok(())
}
```

## Ejecución

### Compilar y ejecutar

```bash
cd userland
cargo build --bin eclipse_userland
cargo run --bin eclipse_userland
```

### Salida esperada

```
╔══════════════════════════════════════════════════════════════════════╗
║         Eclipse OS - Userland con Servidores Microkernel           ║
║                    Servicios en Espacio de Usuario                  ║
╚══════════════════════════════════════════════════════════════════════╝

═══════════════════════════════════════════════════════
  Inicializando Servidores del Microkernel (Userspace)
═══════════════════════════════════════════════════════

Registrando servidores del microkernel...
   ✓ Registrando servidor: Security
   ✓ Registrando servidor: FileSystem
   ✓ Registrando servidor: Graphics
   ✓ Registrando servidor: Network
   ✓ Registrando servidor: Input
   ✓ Registrando servidor: Audio
   ✓ Registrando servidor: AI

Inicializando servidores registrados...
   [SEC] Inicializando servidor de seguridad...
   [FS] Inicializando servidor de sistema de archivos...
   [GFX] Inicializando servidor de gráficos...
   [NET] Inicializando servidor de red...
   [INPUT] Inicializando servidor de entrada...
   [AUDIO] Inicializando servidor de audio...
   [AI] Inicializando servidor de IA...

✅ Todos los servidores del microkernel inicializados
```

## Estadísticas

Cada servidor mantiene estadísticas de operación:

```rust
pub struct ServerStats {
    pub messages_processed: u64,
    pub messages_failed: u64,
    pub uptime_seconds: u64,
    pub last_error: Option<String>,
}
```

Ver estadísticas con:
```rust
service_manager.show_microkernel_stats();
```

## Formato de Mensajes

Los mensajes entre kernel y servidores usan esta estructura:

```rust
#[repr(C)]
pub struct Message {
    pub id: u64,                    // ID único del mensaje
    pub from: u32,                  // ID del cliente
    pub to: u32,                    // ID del servidor
    pub message_type: MessageType,  // Tipo de mensaje
    pub data: [u8; 256],           // Datos del mensaje
    pub data_size: u32,            // Tamaño de datos válidos
    pub priority: u8,              // Prioridad del mensaje
    pub flags: u8,                 // Flags de control
    pub reserved: [u8; 2],         // Reservado
}
```

## Próximos Pasos

1. **Conectar con kernel**: Implementar comunicación IPC real entre kernel y servidores userspace
2. **Persistencia**: Añadir almacenamiento persistente de configuración
3. **Optimización**: Optimizar procesamiento de mensajes para alta carga
4. **Testing**: Añadir tests unitarios y de integración
5. **Documentación**: Expandir documentación de API de cada servidor

## Referencias

- [MICROKERNEL_ARCHITECTURE.md](../MICROKERNEL_ARCHITECTURE.md) - Arquitectura completa del microkernel
- [README.md](../README.md) - Documentación general de Eclipse OS
- Código fuente: `userland/src/services/servers/`

## Licencia

Este código es parte de Eclipse OS y está bajo licencia MIT.
