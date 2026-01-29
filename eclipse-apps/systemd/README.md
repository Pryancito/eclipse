# Eclipse SystemD v0.1.0

Sistema de inicialización moderno y completo para Eclipse OS que implementa funcionalidades avanzadas similares a systemd.

## Características Principales

### Arquitectura del Sistema
- Sistema modular: Arquitectura completamente modular con separación de responsabilidades
- Integración completa: Integración nativa con el kernel Eclipse OS
- **Monitoreo de procesos real**: Monitoreo de salud de procesos usando `/proc/<pid>/stat`
- Gestión de recursos: Control de CPU, memoria e I/O por servicio
- Sistema de notificaciones: Notificaciones en tiempo real entre servicios
- **Uptime tracking**: Seguimiento preciso del tiempo de actividad del sistema desde el arranque

### Gestión de Servicios
- Parser completo: Parser robusto de archivos `.service` estándar
- Validador avanzado: Validación completa de sintaxis y dependencias
- Estados del servicio: Estados completos (inactive, activating, active, deactivating, failed)
- Control de ciclo de vida: Inicio, parada, reinicio y recarga de servicios
- Manejo de señales: SIGTERM graceful shutdown con fallback a SIGKILL
- **Restart Policy**: Implementación completa de políticas de reinicio (`always`, `on-failure`, `on-abnormal`)
- **Auto-restart**: Reinicio automático de servicios fallidos con límite de reintentos (máximo 5)
- **RestartSec**: Soporte para tiempo de espera configurable antes de reiniciar

### Sistema de Dependencias
- Resolución inteligente: Resolución automática de dependencias con detección de ciclos
- Tipos de dependencia: `Requires`, `Wants`, `After`, `Before`, `Conflicts`
- Orden de inicio: Ordenamiento topológico para inicio correcto
- Validación: Verificación automática de dependencias faltantes

### Monitoreo y Logging
- Journal estructurado: Sistema de logging con JSON estructurado
- Rotación automática: Rotación de archivos con compresión gzip
- Niveles de prioridad: Emergencia, Alerta, Crítico, Error, Warning, Notice, Info, Debug
- Búsqueda avanzada: Búsqueda y filtrado de logs por servicio y prioridad
- Compresión: Compresión automática con niveles configurables

### Sistema de Notificaciones
- Canales broadcast: Comunicación en tiempo real entre servicios
- Tipos de notificación: Ready, Reloading, Stopping, Error, Custom
- Historial: Historial completo de notificaciones con límites configurables
- Suscripción: Sistema de suscripción/desuscripción a canales

### Gestión de Recursos
- Monitoreo de CPU: Uso de CPU por proceso y sistema
- Monitoreo de memoria: RAM, cache y buffers del sistema
- Monitoreo de I/O: Operaciones de lectura/escritura por proceso
- Límites configurables: Límites de CPU, memoria e I/O por servicio
- Historial de uso: Historial temporal de uso de recursos

### Gestión de Targets
- Sistema de targets: Equivalente moderno a los runlevels tradicionales
- Dependencias: Resolución de dependencias entre targets
- Estados: Estados completos para targets (active, inactive, failed)
- Transiciones: Transiciones suaves entre targets

### Interfaz de Control
- systemctl: Interfaz de línea de comandos completa
- Comandos principales: start, stop, restart, reload, status, enable, disable
- Gestión de targets: set-default, get-default, isolate
- Monitoreo: list-units, list-services, show
- Ayuda integrada: Sistema de ayuda completo

## Instalación

### Compilación desde fuente
```bash
# Clonar el repositorio
cd eclipse-apps/systemd

# Compilar en modo release
cargo build --release

# Ejecutar pruebas
cargo test

# Verificar compilación
ls -la target/release/eclipse-systemd
```

### Instalación del sistema
```bash
# Instalar (requiere sudo)
sudo ./install_systemd.sh

# Verificar instalación
sudo service eclipse-systemd status
```

### Integración con kernel
```bash
# El kernel Eclipse OS incluye soporte de integración systemd
# Ubicado en: eclipse_kernel/src/init_system.rs

# Estado de la integración kernel-systemd:
# ✅ Módulo init_system.rs implementado
# ✅ Hook de inicialización en kernel_main()
# ✅ Configuración de PID 1 y variables de entorno
# ⚠️ Carga de ELF (simulada - requiere VFS real)
# ⚠️ Memoria virtual (simulada - requiere paginación completa)
# ⚠️ Transferencia de control (pendiente - requiere implementación completa)

# Para habilitar systemd en el kernel:
# El kernel verifica automáticamente si systemd debe iniciarse
# Actualmente retorna al kernel loop si falla la transferencia
```

## Uso

### Ejecutar Eclipse SystemD
```bash
# Ejecutar directamente
/sbin/eclipse-systemd

# Como servicio del sistema
sudo service eclipse-systemd start
sudo service eclipse-systemd stop
sudo service eclipse-systemd restart
sudo service eclipse-systemd status
```

### Archivos de configuración

Los archivos .service se encuentran en `/etc/eclipse/systemd/system/`:

- `eclipse-gui.service` - Interfaz gráfica de Eclipse OS
- `network.service` - Gestión de red
- `syslog.service` - Sistema de logging
- `eclipse-shell.service` - Terminal de Eclipse OS

### Targets disponibles

- `basic.target` - Sistema básico
- `multi-user.target` - Sistema multi-usuario
- `graphical.target` - Interfaz gráfica

## Estructura del proyecto

```
eclipse-apps/systemd/
├── src/
│   ├── main.rs              # Aplicación principal
│   └── service_parser.rs    # Parser de archivos .service
├── Cargo.toml               # Configuración del proyecto
├── install_systemd.sh       # Script de instalación
└── README.md               # Este archivo
```

## Archivos .service

Eclipse SystemD soporta archivos .service estándar con las siguientes secciones:

### [Unit]
- `Description` - Descripción del servicio
- `After` - Servicios que deben iniciarse antes
- `Requires` - Dependencias obligatorias
- `Wants` - Dependencias opcionales
- `Conflicts` - Servicios incompatibles

### [Service]
- `Type` - Tipo de servicio (simple, forking, oneshot, dbus, notify, idle)
- `ExecStart` - Comando de inicio
- `ExecReload` - Comando de recarga
- `Restart` - Política de reinicio
- `RestartSec` - Tiempo de espera antes de reiniciar
- `User` - Usuario del servicio
- `Group` - Grupo del servicio
- `WorkingDirectory` - Directorio de trabajo
- `Environment` - Variables de entorno

### [Install]
- `WantedBy` - Target que quiere este servicio
- `RequiredBy` - Target que requiere este servicio

## Ejemplo de archivo .service

```ini
[Unit]
Description=Eclipse OS Graphical User Interface
Documentation=https://eclipse-os.dev/gui
After=network.service
Wants=network.service
Requires=basic.target

[Service]
Type=notify
ExecStart=/sbin/eclipse-gui
ExecReload=/bin/kill -HUP $MAINPID
Restart=on-failure
RestartSec=5
User=root
Group=root
WorkingDirectory=/
Environment=DISPLAY=:0
Environment=XDG_SESSION_TYPE=wayland

[Install]
WantedBy=graphical.target
```

## Desarrollo

### Compilar
```bash
cargo build
cargo build --release
```

### Ejecutar
```bash
cargo run
```

### Tests
```bash
cargo test
```

## Dependencias

- `anyhow` - Manejo de errores
- `log` - Sistema de logging
- `env_logger` - Logger para entorno
- `serde` - Serialización
- `tokio` - Runtime asíncrono
- `chrono` - Manejo de fechas y tiempos
- `uuid` - Generación de UUIDs

## Integración con Eclipse OS

Eclipse SystemD está diseñado para integrarse con el kernel Eclipse:

1. **Arranque**: El kernel ejecuta `/sbin/init` (enlace a eclipse-systemd)
2. **Servicios**: Carga y ejecuta servicios desde archivos .service
3. **Targets**: Inicia el target apropiado (multi-user o graphical)
4. **Monitoreo**: Monitorea servicios y los reinicia si fallan

## Estado del proyecto

### ✅ Completado
- Parser de archivos .service
- Validador de sintaxis
- Gestión básica de servicios
- Sistema de logging (journal)
- Script de instalación
- **Monitoreo de procesos real** (usando /proc filesystem)
- **Restart Policy** (always, on-failure, on-abnormal)
- **Auto-restart de servicios** con límite de reintentos
- **Uptime tracking** desde boot
- Gestión avanzada de dependencias
- Sistema de notificaciones
- Gestión de recursos (monitoreo)
- **Integración con kernel Eclipse OS** (módulo init_system.rs)

### 🚧 En Progreso
- Integración completa con kernel (requiere VFS y paginación)
- Carga real de ejecutables ELF desde filesystem
- Transferencia de control kernel→userland
- Implementación de syscalls críticas (fork, exec, wait)
- Privilege dropping (User/Group directives)
- inotify para detección de cambios en archivos .service
- Aplicación de límites usando cgroups

### 🔧 Limitaciones Actuales de la Integración Kernel

El kernel Eclipse OS tiene un módulo `init_system.rs` que proporciona la
infraestructura para ejecutar eclipse-systemd como PID 1, pero actualmente
tiene las siguientes limitaciones:

1. **Filesystem**: No hay VFS funcional, por lo que la carga de ejecutables
   usa datos ELF ficticios en lugar de leer `/sbin/init` del disco.

2. **Memoria Virtual**: El mapeo de memoria es simulado y no configura
   tablas de páginas reales para el espacio de usuario.

3. **Transferencia de Control**: La función `iretq` está documentada pero
   no se ejecuta realmente porque requiere paginación completa.

4. **Syscalls**: Las syscalls críticas (fork, exec, wait, signal) no están
   implementadas, lo que impide que systemd cree y gestione procesos.

Cuando estas limitaciones se resuelvan, el kernel podrá transferir
completamente el control a eclipse-systemd y el sistema operativo
funcionará con un init system completo.

### ⏳ Planificado
- Soporte para sockets systemd
- Timer units (.timer files)
- Path units (.path files)
- Soporte completo para D-Bus
- Mejor manejo de SIGTERM/SIGKILL

## Contribuir

1. Fork el proyecto
2. Crea una rama para tu feature
3. Commit tus cambios
4. Push a la rama
5. Abre un Pull Request

## Licencia

Este proyecto está bajo la licencia MIT. Ver `LICENSE` para más detalles.

## Soporte

Para soporte y preguntas:
- GitHub Issues: [eclipse-os/issues](https://github.com/eclipse-os/issues)
- Documentación: [eclipse-os.dev](https://eclipse-os.dev)
- Email: support@eclipse-os.dev
