# Eclipse OS - Arquitectura Microkernel

## Descripción General

Eclipse OS ha sido convertido de un kernel híbrido a una arquitectura microkernel pura. En esta arquitectura, el kernel mantiene únicamente las funciones más esenciales, mientras que todos los servicios del sistema se ejecutan como servidores en espacio de usuario.

## Principios del Microkernel

### Responsabilidades del Kernel
El kernel Eclipse solo maneja:
- **Gestión de Memoria**: Paginación, heap, asignación de memoria
- **IPC (Inter-Process Communication)**: Sistema de mensajería entre procesos
- **Scheduling Básico**: Planificación de tareas y procesos
- **Interrupciones**: Manejo de interrupciones del hardware

### Servicios en Espacio de Usuario
Todos los demás servicios del sistema se ejecutan como servidores independientes:
- FileSystem Server
- Graphics Server
- Network Server
- Input Server
- Audio Server
- AI Server
- Security Server

## Arquitectura de Mensajería

### Sistema de Mensajes
Cada mensaje en el microkernel contiene:
```rust
pub struct Message {
    pub id: MessageId,           // ID único del mensaje
    pub from: ClientId,          // ID del cliente que envía
    pub to: ServerId,            // ID del servidor destino
    pub message_type: MessageType, // Tipo de mensaje
    pub data: [u8; 256],         // Datos del mensaje
    pub data_size: u32,          // Tamaño de datos válidos
    pub priority: u8,            // Prioridad del mensaje
    pub flags: u8,               // Flags de control
}
```

### Tipos de Mensajes
```rust
pub enum MessageType {
    System = 0x00000001,
    Memory = 0x00000002,
    FileSystem = 0x00000004,
    Network = 0x00000008,
    Graphics = 0x00000010,
    Audio = 0x00000020,
    Input = 0x00000040,
    AI = 0x00000080,
    Security = 0x00000100,
    User = 0x00000200,
}
```

## Servidores del Sistema

### Prioridades de Servidores

| Servidor | Prioridad | Descripción |
|----------|-----------|-------------|
| Security | 10 (Máxima) | Manejo de seguridad y autenticación |
| FileSystem | 10 (Máxima) | Gestión del sistema de archivos |
| Graphics | 9 (Alta) | Renderizado y display |
| Input | 9 (Alta) | Teclado, mouse y otros dispositivos de entrada |
| Network | 8 (Media-Alta) | Stack de red TCP/IP |
| Audio | 7 (Media) | Reproducción y captura de audio |
| AI | 6 (Baja) | Servicios de inteligencia artificial |

### Registro de Servidores
Durante el arranque del kernel, cada servidor se registra en el microkernel:

```rust
if let Some(server_id) = crate::microkernel::register_server(
    b"FileSystem",
    crate::microkernel::MessageType::FileSystem,
    10  // Prioridad alta
) {
    // Servidor registrado exitosamente
} else {
    // Error al registrar servidor
}
```

## Procesamiento de Mensajes

### Main Loop
El loop principal del kernel procesa mensajes del microkernel en cada iteración:

```rust
loop {
    // Ejecutar tareas programadas
    run_scheduled_tasks(fb);
    
    // Procesar mensajes del microkernel
    crate::microkernel::process_messages();
    
    // Otras tareas del sistema...
}
```

### Estadísticas del Microkernel
El sistema mantiene estadísticas en tiempo real:
```rust
pub struct MicrokernelStatistics {
    pub total_messages: u64,
    pub messages_per_second: u32,
    pub active_servers: u32,
    pub active_clients: u32,
    pub memory_usage: usize,
    pub cpu_usage: f32,
    pub uptime: u64,
    pub error_count: u32,
}
```

## Ventajas de la Arquitectura Microkernel

### Seguridad
- **Aislamiento**: Cada servicio se ejecuta en su propio espacio de memoria
- **Privilegios Mínimos**: Los servicios no tienen acceso directo al hardware
- **Contencion de Fallos**: Un fallo en un servicio no afecta al kernel ni a otros servicios

### Modularidad
- **Servicios Intercambiables**: Los servidores pueden ser reemplazados sin modificar el kernel
- **Desarrollo Independiente**: Cada servicio puede desarrollarse y probarse por separado
- **Actualizaciones Dinámicas**: Los servicios pueden actualizarse sin reiniciar el kernel

### Mantenibilidad
- **Código Simplificado**: El kernel es más pequeño y fácil de mantener
- **Debugging Facilitado**: Los problemas se aíslan en servicios específicos
- **Testing Mejorado**: Cada componente puede probarse independientemente

### Escalabilidad
- **Distribución de Carga**: Los servicios pueden ejecutarse en diferentes núcleos
- **Optimización Granular**: Cada servicio puede optimizarse individualmente
- **Recursos Dinámicos**: Asignación de recursos según demanda

## Migración Futura

### Servicios Pendientes de Migración
Los siguientes componentes aún están en el kernel y deben migrarse a servidores userland:
- Drivers de hardware específicos (GPU, USB, etc.)
- Sistema de archivos virtual (VFS)
- Stack de red completo
- Sistema de ventanas

### Plan de Migración
1. **Fase 1** (Completada): Inicializar infraestructura de microkernel
2. **Fase 2**: Mover drivers a servidores userland
3. **Fase 3**: Migrar sistema de archivos a FileSystem Server
4. **Fase 4**: Migrar stack de red a Network Server
5. **Fase 5**: Migrar sistema gráfico a Graphics Server

## Referencia de API

### Funciones Principales del Microkernel

#### Inicialización
```rust
pub fn init_microkernel() -> bool
```
Inicializa el sistema de microkernel. Debe llamarse temprano en el boot.

#### Registro de Servidores
```rust
pub fn register_server(name: &[u8], message_type: MessageType, priority: u8) -> Option<ServerId>
```
Registra un nuevo servidor en el sistema.

#### Registro de Clientes
```rust
pub fn register_client(name: &[u8], server_id: ServerId, permissions: u32) -> Option<ClientId>
```
Registra un cliente que desea comunicarse con un servidor.

#### Envío de Mensajes
```rust
pub fn send_message(from: ClientId, to: ServerId, message_type: MessageType, data: &[u8]) -> bool
```
Envía un mensaje de un cliente a un servidor.

#### Recepción de Mensajes
```rust
pub fn receive_message(server_id: ServerId) -> Option<Message>
```
Recibe el siguiente mensaje en la cola de un servidor.

#### Procesamiento de Mensajes
```rust
pub fn process_messages()
```
Procesa todos los mensajes pendientes en el sistema.

#### Estadísticas
```rust
pub fn get_microkernel_statistics() -> Option<MicrokernelStatistics>
pub fn get_server_statistics(server_id: ServerId) -> Option<ServerStatistics>
pub fn get_client_statistics(client_id: ClientId) -> Option<ClientStatistics>
```
Obtiene estadísticas del microkernel, servidores o clientes.

## Ejemplo de Uso

```rust
// Registrar un servidor de archivos
let fs_server_id = microkernel::register_server(
    b"FileSystem",
    MessageType::FileSystem,
    10
).expect("Failed to register FileSystem server");

// Registrar un cliente
let client_id = microkernel::register_client(
    b"MyApp",
    fs_server_id,
    0x01  // Permisos de lectura
).expect("Failed to register client");

// Enviar un mensaje
let data = b"READ /etc/passwd";
microkernel::send_message(
    client_id,
    fs_server_id,
    MessageType::FileSystem,
    data
);

// En el servidor, recibir el mensaje
if let Some(msg) = microkernel::receive_message(fs_server_id) {
    // Procesar el mensaje
    process_file_request(&msg);
}
```

## Conclusión

La arquitectura microkernel de Eclipse OS proporciona una base sólida para un sistema operativo moderno, seguro y escalable. La separación clara entre el kernel y los servicios del sistema permite un desarrollo más eficiente, mejor aislamiento de fallos y mayor flexibilidad en la evolución del sistema.
