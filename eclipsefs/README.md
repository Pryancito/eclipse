# EclipseFS - Sistema de Archivos para Eclipse OS Microkernel

EclipseFS es el servidor de sistema de archivos en espacio de usuario para Eclipse OS, diseñado para funcionar con la arquitectura microkernel del sistema operativo.

## Descripción

Este servidor implementa todas las operaciones del sistema de archivos EclipseFS y se comunica con el kernel a través del sistema de mensajes IPC (Inter-Process Communication). Está construido siguiendo los principios de la arquitectura microkernel, donde los servicios del sistema se ejecutan en espacio de usuario para mayor seguridad y estabilidad.

## Características

### Operaciones Soportadas

- **Mount/Unmount**: Montar y desmontar sistemas de archivos
- **Open**: Abrir archivos con diferentes modos
- **Read**: Leer datos de archivos
- **Write**: Escribir datos a archivos
- **Close**: Cerrar descriptores de archivo
- **Create**: Crear nuevos archivos
- **Delete**: Eliminar archivos
- **List**: Listar contenido de directorios
- **Stat**: Obtener información de archivos
- **Sync**: Sincronizar cambios al disco

### Características Avanzadas

- **Gestión de File Descriptors**: Tabla de descriptores de archivo abiertos
- **Integración con eclipsefs-lib**: Uso de la biblioteca EclipseFS para operaciones reales
- **Manejo de Errores Robusto**: Sistema de errores completo con estadísticas
- **Arquitectura Microkernel**: Comunicación vía IPC con el kernel
- **Prioridad Alta**: Prioridad 10 para operaciones de sistema de archivos

## Arquitectura

```
eclipsefs/
├── src/
│   ├── lib.rs           # Módulo principal y exports
│   ├── main.rs          # Punto de entrada del servidor
│   ├── server.rs        # Implementación del servidor EclipseFS
│   ├── messages.rs      # Definiciones de mensajes IPC
│   └── operations.rs    # Operaciones del filesystem
└── Cargo.toml          # Dependencias y configuración
```

### Componentes

1. **EclipseFSServer**: Servidor principal que implementa el trait MicrokernelServer
2. **FileSystemOperations**: Gestiona todas las operaciones del filesystem
3. **Message System**: Sistema de mensajes compatible con el microkernel
4. **File Descriptors**: Tabla de descriptores de archivo abiertos

## Uso

### Compilar

```bash
cd eclipsefs
cargo build --release
```

### Ejecutar

```bash
# Ejecutar el servidor
cargo run --release

# O ejecutar el binario directamente
./target/release/eclipsefs-server
```

### Usar como Biblioteca

```rust
use eclipsefs::{EclipseFSServer, MicrokernelServer};

let mut server = EclipseFSServer::new();
server.initialize()?;

// Procesar mensajes...
let response = server.process_message(&message)?;

server.shutdown()?;
```

## Integración con el Microkernel

El servidor EclipseFS se integra con el microkernel Eclipse OS de la siguiente manera:

1. **Registro**: El servidor se registra en el microkernel durante el arranque
2. **Mensajes**: Recibe mensajes del tipo `MessageType::FileSystem`
3. **Prioridad**: Opera con prioridad 10 (alta)
4. **IPC**: Se comunica con otros componentes vía sistema de mensajes

### Ejemplo de Mensaje

```rust
let message = Message {
    id: 1,
    from: CLIENT_ID,
    to: FILESYSTEM_SERVER_ID,
    message_type: MessageType::FileSystem,
    data: [COMMAND_BYTE, ...COMMAND_DATA...],
    data_size: size,
    priority: 10,
    flags: 0,
    reserved: [0; 2],
};
```

## Comandos

Los comandos se envían como el primer byte del campo `data` del mensaje:

| Comando | Código | Descripción |
|---------|--------|-------------|
| Open    | 1      | Abrir archivo |
| Read    | 2      | Leer datos |
| Write   | 3      | Escribir datos |
| Close   | 4      | Cerrar archivo |
| Create  | 5      | Crear archivo |
| Delete  | 6      | Eliminar archivo |
| List    | 7      | Listar directorio |
| Stat    | 8      | Info de archivo |
| Mkdir   | 9      | Crear directorio |
| Rmdir   | 10     | Eliminar directorio |
| Rename  | 11     | Renombrar |
| Chmod   | 12     | Cambiar permisos |
| Sync    | 13     | Sincronizar |
| StatFS  | 14     | Info del FS |
| Mount   | 15     | Montar FS |
| Unmount | 16     | Desmontar FS |

## Dependencias

- **anyhow**: Manejo de errores
- **eclipsefs-lib**: Biblioteca del sistema de archivos EclipseFS

## Características del Diseño

### Siguiendo el Estilo del Kernel

Este servidor está diseñado siguiendo las convenciones de la arquitectura microkernel de Eclipse OS:

1. **Separación de Concerns**: El servidor solo maneja operaciones de filesystem
2. **Comunicación IPC**: Todo se comunica a través de mensajes
3. **Userspace**: Se ejecuta completamente en espacio de usuario
4. **Trait-based**: Usa el trait `MicrokernelServer` para consistencia
5. **Estadísticas**: Mantiene estadísticas de operación y errores

### Seguridad

- Validación de descriptores de archivo
- Verificación de filesystem montado antes de operaciones
- Manejo de errores robusto
- Límites en tamaños de buffer

### Performance

- Gestión eficiente de descriptores de archivo con HashMap
- Integración con sistema de cache de eclipsefs-lib
- Operaciones sincronizadas para consistencia de datos

## Desarrollo

### Tests

```bash
cargo test
```

### Formato

```bash
cargo fmt
```

### Lint

```bash
cargo clippy
```

## Roadmap

- [ ] Implementar todos los comandos (Mkdir, Rmdir, Rename, Chmod)
- [ ] Agregar soporte para permisos y ACLs
- [ ] Implementar cache de metadatos
- [ ] Soporte para múltiples filesystems montados simultáneamente
- [ ] Optimizaciones de rendimiento
- [ ] Tests unitarios completos
- [ ] Tests de integración con el kernel

## Licencia

MIT License - Ver LICENSE para más detalles

## Contribuir

Ver CONTRIBUTING.md en el repositorio principal de Eclipse OS.

## Autor

Eclipse OS Team
